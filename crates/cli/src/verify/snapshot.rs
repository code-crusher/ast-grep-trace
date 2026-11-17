use crate::lang::SgLang;
use anyhow::{Result, anyhow};
use ast_grep_config::{Label, LabelStyle, RuleConfig};
use ast_grep_core::matcher::MatcherExt;
use ast_grep_core::{
  Doc,
  tree_sitter::{LanguageExt, StrDoc},
};

use super::CaseResult;
use serde::{Deserialize, Serialize, Serializer};

use std::collections::{BTreeMap, HashMap};

type CaseId = String;
type Source = String;

/// A collection of test snapshots for different rules
/// where each [TestSnapshots] is identified by its rule ID.
pub type SnapshotCollection = HashMap<CaseId, TestSnapshots>;

fn merge_snapshots(
  accepted: SnapshotCollection,
  mut existing: SnapshotCollection,
) -> SnapshotCollection {
  for (id, tests) in accepted {
    if let Some(existing) = existing.get_mut(&id) {
      existing.snapshots.extend(tests.snapshots);
    } else {
      existing.insert(id, tests);
    }
  }
  existing
}

/// Represents user's decision when [CaseStatus::Wrong].
/// Snapshot update can be accepted or rejected.
#[derive(Debug)]
pub enum SnapshotAction {
  /// Accept changes
  NeedUpdate,
  /// Reject all changes.
  AcceptNone,
}

impl SnapshotAction {
  pub fn update_snapshot_collection(
    self,
    existing: SnapshotCollection,
    results: &[CaseResult],
  ) -> Option<SnapshotCollection> {
    let accepted = match self {
      Self::NeedUpdate => results
        .iter()
        .map(|result| (result.id.to_string(), result.changed_snapshots()))
        .collect(),
      Self::AcceptNone => return None,
    };
    Some(merge_snapshots(accepted, existing))
  }
}

/// A list of test snapshots for one specific rule-test identified by its `CaseId`.
/// A test yaml for one rule have multiple valid/invalid test cases.
/// Each invalid code test case has its [TestSnapshot].
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TestSnapshots {
  pub id: CaseId,
  #[serde(serialize_with = "ordered_map")]
  pub snapshots: HashMap<Source, TestSnapshot>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TestSnapshot {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub fixed: Option<String>,
  pub labels: Vec<LabelSnapshot>,
}

impl TestSnapshot {
  /// Generate snapshot from rule and test case code
  // Ideally we should return Option<Result<T>>
  // because Some/None indicates if we have found matches,
  // then Result<T> indicates if we have error during replace
  // But to reuse anyhow we use the Result<Option<T>>
  pub fn generate(rule_config: &RuleConfig<SgLang>, case: &str) -> Result<Option<Self>> {
    let mut sg = rule_config.language.ast_grep(case);
    let rule = &rule_config.matcher;
    let Some(matched) = sg.root().find(rule) else {
      return Ok(None);
    };
    let labels = rule_config
      .get_labels(&matched)
      .into_iter()
      .map(LabelSnapshot::from)
      .collect();
    let Some(fix) = rule_config.fixer.first() else {
      return Ok(Some(Self {
        fixed: None,
        labels,
      }));
    };
    // apply the fix to the first match that satisfies the fix constraints
    let matches: Vec<_> = sg.root().dfs().filter_map(|n| rule.match_node(n)).collect();
    let applicable = matches.into_iter().find(|nm| fix.is_applicable(nm));
    let Some(nm) = applicable else {
      return Ok(Some(Self {
        fixed: None,
        labels,
      }));
    };
    let edit = nm.make_edit(rule, fix);
    sg.edit(edit).map_err(|e| anyhow!(e))?;
    Ok(Some(Self {
      fixed: Some(sg.source().to_string()),
      labels,
    }))
  }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LabelSnapshot {
  pub(super) source: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  message: Option<String>,
  style: LabelStyle,
  start: usize,
  end: usize,
}
impl<'r, 't> From<Label<'r, 't, StrDoc<SgLang>>> for LabelSnapshot {
  fn from(label: Label<'r, 't, StrDoc<SgLang>>) -> Self {
    let range = label.range();
    let source = label.start_node.get_doc().get_source();
    Self {
      source: source[range.clone()].to_string(),
      message: label.message.map(ToString::to_string),
      style: label.style,
      start: range.start,
      end: range.end,
    }
  }
}

fn ordered_map<S>(value: &HashMap<String, TestSnapshot>, serializer: S) -> Result<S::Ok, S::Error>
where
  S: Serializer,
{
  let ordered: BTreeMap<_, _> = value.iter().collect();
  ordered.serialize(serializer)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::verify::test::{TEST_RULE, get_rule_config};

  fn get_fix_constraint_rule_config() -> RuleConfig<SgLang> {
    use ast_grep_config::{GlobalRules, from_str};
    let globals = GlobalRules::default();
    let yaml = format!(
      "
id: {TEST_RULE}
message: test
severity: hint
language: TypeScript
rule:
  pattern: log($A)
fix:
  template: 'logger($A)'
  constraints:
    A: {{kind: number}}
"
    );
    let inner: ast_grep_config::SerializableRuleConfig<SgLang> = from_str(&yaml).unwrap();
    RuleConfig::try_from(inner, &globals).unwrap()
  }

  #[test]
  fn test_generate_fix_constraints() -> Result<()> {
    let rule_config = get_fix_constraint_rule_config();
    // constraint satisfied: the fix is applied
    let result = TestSnapshot::generate(&rule_config, "log(123);")?;
    let snapshot = result.expect("rule should match");
    assert_eq!(snapshot.fixed.as_deref(), Some("logger(123);"));
    // constraint violated: rule still matches but no fix is recorded
    let result = TestSnapshot::generate(&rule_config, "log(a);")?;
    let snapshot = result.expect("rule should match");
    assert_eq!(snapshot.fixed, None);
    Ok(())
  }

  #[test]
  fn test_generate() -> Result<()> {
    let rule_config = get_rule_config("pattern: let x = $A");
    let case = "let x = 42;";
    let result = TestSnapshot::generate(&rule_config, case)?;
    assert_eq!(
      result,
      Some(TestSnapshot {
        fixed: None,
        labels: vec![LabelSnapshot {
          source: "let x = 42;".into(),
          message: None,
          style: LabelStyle::Primary,
          start: 0,
          end: 11,
        }]
      })
    );
    Ok(())
  }

  #[test]
  fn test_not_found() -> Result<()> {
    let rule_config = get_rule_config("pattern: var x = $A");
    let case = "let x = 42;";
    let result = TestSnapshot::generate(&rule_config, case)?;
    assert_eq!(result, None,);
    Ok(())
  }

  #[test]
  fn test_secondary_label() -> Result<()> {
    let rule_config =
      get_rule_config("{pattern: 'let x = $A;', inside: {kind: 'statement_block'}}");
    let case = "function test() { let x = 42; }";
    let result = TestSnapshot::generate(&rule_config, case)?;
    assert_eq!(
      result,
      Some(TestSnapshot {
        fixed: None,
        labels: vec![
          LabelSnapshot {
            source: "let x = 42;".into(),
            message: None,
            style: LabelStyle::Primary,
            start: 18,
            end: 29,
          },
          LabelSnapshot {
            source: "{ let x = 42; }".into(),
            message: None,
            style: LabelStyle::Secondary,
            start: 16,
            end: 31
          }
        ],
      })
    );
    Ok(())
  }

  #[test]
  fn test_snapshot_action() -> Result<()> {
    use crate::verify::CaseStatus;
    let action = SnapshotAction::NeedUpdate;
    let rule_config = get_rule_config("pattern: let x = $A");
    let sc = SnapshotCollection::new();
    let op = action
      .update_snapshot_collection(
        sc,
        &[CaseResult {
          id: TEST_RULE,
          cases: vec![CaseStatus::Updated {
            source: "let x = 123",
            updated: TestSnapshot::generate(&rule_config, "let x = 123")?.unwrap(),
          }],
        }],
      )
      .expect("should have new op");
    assert_eq!(
      op[TEST_RULE].snapshots["let x = 123"].labels[0].source,
      "let x = 123"
    );
    Ok(())
  }
}
