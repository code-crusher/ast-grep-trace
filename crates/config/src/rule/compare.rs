use super::deserialize_env::DeserializeEnv;
use crate::rule::RuleSerializeError;

use ast_grep_core::language::Language;
use ast_grep_core::matcher::Matcher;
use ast_grep_core::meta_var::{MetaVarEnv, MetaVariable};
use ast_grep_core::source::Content;
use ast_grep_core::{Doc, Node};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use bit_set::BitSet;
use std::borrow::Cow;

/// A rule object to match a node by comparing its text with a meta variable's.
///
/// This is most useful in `constraints`, where the meta variables matched by
/// `rule` are all available and can be compared against each other.
///
/// ```yaml
/// rule:
///   pattern: let($SYMBOL) { $$$_ }
///   precedes:
///     stopBy: end
///     pattern: before { $METHOD }
/// constraints:
///   SYMBOL:
///     compare:
///       source: ":(.*)"
///       target: $METHOD
/// ```
#[derive(Serialize, Deserialize, Clone, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SerializableCompare {
  /// A regex to extract the text for comparison. Its first capture group is
  /// used; a regex without a capture group compares the whole match.
  /// Defaults to the whole text of the node.
  #[serde(default, with = "serde_regex")]
  #[schemars(with = "String")]
  pub source: Option<regex::Regex>,
  /// The `$VAR` whose matched text is compared against.
  pub target: String,
}

pub struct Compare {
  source: Option<regex::Regex>,
  target: MetaVariable,
}

impl Compare {
  pub fn try_new<L: Language>(
    cmp: SerializableCompare,
    env: &DeserializeEnv<L>,
  ) -> Result<Self, RuleSerializeError> {
    let target = env
      .lang
      .extract_meta_var(&env.lang.pre_process_pattern(&cmp.target))
      .ok_or_else(|| RuleSerializeError::InvalidCompare(cmp.target.clone()))?;
    Ok(Self {
      source: cmp.source,
      target,
    })
  }

  /// Extracts the text for comparison via the optional source regex.
  fn extract<'t>(&self, text: &'t str) -> Option<&'t str> {
    match &self.source {
      Some(regex) => {
        let captures = regex.captures(text)?;
        captures
          .get(1)
          .or_else(|| captures.get(0))
          .map(|m| m.as_str())
      }
      None => Some(text),
    }
  }

  /// The meta variable this rule references but does not define.
  pub fn used_vars(&self) -> &str {
    use MetaVariable as MV;
    match &self.target {
      MV::Capture(v, _) => v,
      MV::MultiCapture(v) => v,
      MV::Dropped(_) | MV::Multiple => "",
    }
  }
}

impl Matcher for Compare {
  fn match_node_with_env<'tree, D: Doc>(
    &self,
    node: Node<'tree, D>,
    env: &mut Cow<MetaVarEnv<'tree, D>>,
  ) -> Option<Node<'tree, D>> {
    let target = env.get_var_bytes(&self.target)?;
    let target = <D::Source as Content>::encode_bytes(target);
    let text = node.text();
    let extracted = self.extract(&text)?;
    (extracted == target).then_some(node)
  }

  fn potential_kinds(&self) -> Option<BitSet> {
    None
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::test::TypeScript;
  use crate::{DeserializeEnv, from_str};
  use ast_grep_core::tree_sitter::LanguageExt;

  #[test]
  fn test_simple_compare_deserialize() {
    let rule: crate::rule::SerializableRule = from_str(
      r"
compare:
  target: $A
",
    )
    .expect("should parse");
    let env = DeserializeEnv::new(TypeScript::Tsx);
    let rule = env.deserialize_rule(rule).expect("should deserialize");
    assert!(matches!(rule, crate::rule::Rule::Compare(_)));
  }

  #[test]
  fn test_invalid_compare_var() {
    let rule: crate::rule::SerializableRule = from_str(
      r"
compare:
  target: WRONG
",
    )
    .expect("should parse");
    let env = DeserializeEnv::new(TypeScript::Tsx);
    let err = match env.deserialize_rule(rule) {
      Err(e) => e,
      Ok(_) => panic!("should be an error"),
    };
    assert!(matches!(err, RuleSerializeError::InvalidCompare(wrong) if wrong == "WRONG"));
  }

  #[test]
  fn test_undefined_compare_target_var() {
    // a compare target must be defined by the rule or constraints
    let env = DeserializeEnv::new(TypeScript::Tsx);
    let rule: crate::SerializableRuleCore = from_str(
      r"
rule:
  pattern: $A = $B
constraints:
  B:
    compare:
      target: $UNDEFINED
",
    )
    .expect("should parse");
    let err = match rule.get_matcher(env) {
      Err(e) => e,
      Ok(_) => panic!("should be an error"),
    };
    assert!(matches!(
      err,
      crate::RuleCoreError::UndefinedMetaVar(name, section) if name == "UNDEFINED" && section == "compare"
    ));
  }

  #[test]
  fn test_defined_compare_target_var_passes() {
    // referencing a var bound in the constraint's own pattern is fine
    let env = DeserializeEnv::new(TypeScript::Tsx);
    let rule: crate::SerializableRuleCore = from_str(
      r"
rule:
  pattern: foo($A)
constraints:
  A:
    all:
      - pattern: bar($C)
      - compare:
          target: $C
",
    )
    .expect("should parse");
    rule.get_matcher(env).expect("should build");
  }

  #[test]
  fn test_compare_equality() {
    let yaml = crate::test::make_rule_config(
      "rule",
      "
  pattern: $A = $B
",
    ) + "\nconstraints:\n  B:\n    compare:\n      target: $A\n";
    crate::test::test_rule_match(&yaml, "a = a;");
    crate::test::test_rule_unmatch(&yaml, "a = b;");
  }

  #[test]
  fn test_compare_regex_extraction() {
    // `'(.*)'` strips the quotes of a string literal before comparing
    let yaml = crate::test::make_rule_config(
      "rule",
      "
  pattern: pair($A, $B)
",
    ) + "\nconstraints:\n  B:\n    compare:\n      source: \"'(.*)'\"\n      target: $A\n";
    let config = &crate::from_yaml_string::<TypeScript>(&yaml, &Default::default())
      .expect("rule should parse")[0];
    let grep = TypeScript::Tsx.ast_grep("pair(a, 'a')");
    assert!(grep.root().find(&config.matcher).is_some());
    let grep = TypeScript::Tsx.ast_grep("pair(a, 'b')");
    assert!(grep.root().find(&config.matcher).is_none());
    // target text without quotes does not match the extracted text
    let grep = TypeScript::Tsx.ast_grep("pair(a, a)");
    assert!(grep.root().find(&config.matcher).is_none());
  }

  // The rule from issue #2622: a `let(:sym)` RSpec binding referenced
  // inside `before { sym }` instead of using `let!`. Without `compare`,
  // `:sym` (simple_symbol, text includes the colon) can never equal the
  // `sym` identifier in the `before` block.
  #[test]
  fn test_issue_2622_rspec_let_before() {
    use ast_grep_language::{LanguageExt, SupportLang};
    let yaml = r#"
id: no-let-before
language: Ruby
severity: error
message: "Don't reference a `let` var inside `before`."
rule:
  pattern: let($SYMBOL) { $$$_ }
  precedes:
    stopBy: end
    pattern: before { $METHOD }
constraints:
  SYMBOL:
    compare:
      source: ":(.*)"
      target: $METHOD
"#;
    let config = &crate::from_yaml_string::<SupportLang>(yaml, &Default::default())
      .expect("rule should parse")[0];
    let grep =
      SupportLang::Ruby.ast_grep("let(:something) { create(:something) }\n\nbefore { something }");
    assert!(grep.root().find(&config.matcher).is_some());

    let grep =
      SupportLang::Ruby.ast_grep("let(:something) { create(:something) }\n\nbefore { other }");
    assert!(grep.root().find(&config.matcher).is_none());

    // let! is fine and should not be flagged
    let grep =
      SupportLang::Ruby.ast_grep("let!(:something) { create(:something) }\n\nbefore { something }");
    assert!(grep.root().find(&config.matcher).is_none());
  }
}
