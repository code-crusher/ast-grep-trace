use crate::DeserializeEnv;
use crate::maybe::Maybe;
use crate::rule::{Relation, Rule, RuleSerializeError, StopBy};
use crate::transform::{Transformation, parse_meta_var};
use ast_grep_core::meta_var::MetaVariable;
use ast_grep_core::replacer::{Content, Replacer, TemplateFix, TemplateFixError};
use ast_grep_core::{Doc, Language, Matcher, NodeMatch};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// A pattern string or fix object to auto fix the issue.
/// It can reference metavariables appeared in rule.
#[derive(Serialize, Deserialize, Clone, JsonSchema)]
#[serde(untagged)]
pub enum SerializableFixer {
  Str(String),
  Config(Box<SerializableFixConfig>),
  List(Vec<SerializableFixConfig>),
}

#[derive(Serialize, Deserialize, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SerializableFixConfig {
  template: String,
  #[serde(default, skip_serializing_if = "Maybe::is_absent")]
  expand_end: Maybe<Relation>,
  #[serde(default, skip_serializing_if = "Maybe::is_absent")]
  expand_start: Maybe<Relation>,
  /// A metavariable that identifies the code to be replaced by the fix.
  /// By default, the fix replaces the matched node. When set, the fix
  /// will replace the code captured by this metavariable instead,
  /// e.g. a node bound by a relational rule in `rule`.
  #[serde(default, skip_serializing_if = "Maybe::is_absent")]
  source: Maybe<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  title: Option<String>,
}

#[derive(Debug, Error)]
pub enum FixerError {
  #[error("Fixer template is invalid.")]
  InvalidTemplate(#[from] TemplateFixError),
  #[error("Fixer expansion contains invalid rule.")]
  WrongExpansion(#[from] RuleSerializeError),
  #[error("Fixer source contains invalid metavariable.")]
  InvalidSource(#[from] crate::transform::TransformError),
  #[error("Rewriter must have exactly one fixer.")]
  InvalidRewriter,
  #[error("Fixer in list must have title.")]
  MissingTitle,
}

struct Expansion {
  matches: Rule,
  stop_by: StopBy,
}

impl Expansion {
  fn parse<L: Language>(
    relation: &Maybe<Relation>,
    env: &DeserializeEnv<L>,
  ) -> Result<Option<Self>, FixerError> {
    let inner = match relation {
      Maybe::Absent => return Ok(None),
      Maybe::Present(r) => r.clone(),
    };
    let stop_by = StopBy::try_from(inner.stop_by, env)?;
    let matches = env.deserialize_rule(inner.rule)?;
    Ok(Some(Self { matches, stop_by }))
  }
}

pub struct Fixer {
  template: TemplateFix,
  expand_start: Option<Expansion>,
  expand_end: Option<Expansion>,
  /// When set, the fix replaces the code bound to this metavariable
  /// instead of the matched node.
  source: Option<MetaVariable>,
  title: Option<String>,
}

impl Fixer {
  fn do_parse<L: Language>(
    serialized: &SerializableFixConfig,
    env: &DeserializeEnv<L>,
    transform: &Option<HashMap<String, Transformation>>,
  ) -> Result<Self, FixerError> {
    let SerializableFixConfig {
      template: fix,
      expand_end,
      expand_start,
      source,
      title,
    } = serialized;
    let expand_start = Expansion::parse(expand_start, env)?;
    let expand_end = Expansion::parse(expand_end, env)?;
    let source = match source {
      Maybe::Absent => None,
      Maybe::Present(source) => Some(parse_meta_var(source, &env.lang)?),
    };
    let template = if let Some(trans) = transform {
      let keys: Vec<_> = trans.keys().cloned().collect();
      TemplateFix::with_transform(fix, &env.lang, &keys)
    } else {
      TemplateFix::try_new(fix, &env.lang)?
    };
    Ok(Self {
      template,
      expand_start,
      expand_end,
      source,
      title: title.clone(),
    })
  }

  pub fn parse<L: Language>(
    fixer: &SerializableFixer,
    env: &DeserializeEnv<L>,
    transform: &Option<HashMap<String, Transformation>>,
  ) -> Result<Vec<Self>, FixerError> {
    let ret = match fixer {
      SerializableFixer::Str(fix) => Self::with_transform(fix, env, transform),
      SerializableFixer::Config(cfg) => Self::do_parse(cfg, env, transform),
      SerializableFixer::List(list) => {
        return Self::parse_list(list, env, transform);
      }
    };
    Ok(vec![ret?])
  }

  fn parse_list<L: Language>(
    list: &[SerializableFixConfig],
    env: &DeserializeEnv<L>,
    transform: &Option<HashMap<String, Transformation>>,
  ) -> Result<Vec<Self>, FixerError> {
    list
      .iter()
      .map(|cfg| {
        if cfg.title.is_none() {
          return Err(FixerError::MissingTitle);
        }
        Self::do_parse(cfg, env, transform)
      })
      .collect()
  }

  pub(crate) fn with_transform<L: Language>(
    fix: &str,
    env: &DeserializeEnv<L>,
    transform: &Option<HashMap<String, Transformation>>,
  ) -> Result<Self, FixerError> {
    let template = if let Some(trans) = transform {
      let keys: Vec<_> = trans.keys().cloned().collect();
      TemplateFix::with_transform(fix, &env.lang, &keys)
    } else {
      TemplateFix::try_new(fix, &env.lang)?
    };
    Ok(Self {
      template,
      expand_end: None,
      expand_start: None,
      source: None,
      title: None,
    })
  }

  pub fn from_str<L: Language>(src: &str, lang: &L) -> Result<Self, FixerError> {
    let template = TemplateFix::try_new(src, lang)?;
    Ok(Self {
      template,
      expand_start: None,
      expand_end: None,
      source: None,
      title: None,
    })
  }

  pub fn title(&self) -> Option<&str> {
    self.title.as_deref()
  }

  pub(crate) fn used_vars(&self) -> HashSet<&str> {
    let mut vars = self.template.used_vars();
    if let Some(source) = &self.source {
      let name = match source {
        MetaVariable::Capture(v, _) | MetaVariable::MultiCapture(v) => v,
        // Dropped/Multiple are unnamed and cannot be parsed from a named
        // metavariable string here, but handle them defensively anyway.
        MetaVariable::Dropped(_) | MetaVariable::Multiple => return vars,
      };
      vars.insert(name);
    }
    vars
  }
}

impl<D, C> Replacer<D> for Fixer
where
  D: Doc<Source = C>,
  C: Content,
{
  fn generate_replacement(&self, nm: &NodeMatch<'_, D>) -> Vec<C::Underlying> {
    // simple forwarding to template
    self.template.generate_replacement(nm)
  }
  fn get_replaced_range(&self, nm: &NodeMatch<'_, D>, matcher: impl Matcher) -> Range<usize> {
    if let Some(range) = self.get_source_range(nm) {
      return range;
    }
    let range = nm.range();
    if self.expand_start.is_none() && self.expand_end.is_none() {
      return if let Some(len) = matcher.get_match_len(nm.get_node().clone()) {
        range.start..range.start + len
      } else {
        range
      };
    }
    let start = expand_start(self.expand_start.as_ref(), nm);
    let end = expand_end(self.expand_end.as_ref(), nm);
    start..end
  }
}

impl Fixer {
  /// Returns the range of the code captured by the fix's `source`
  /// metavariable, or `None` when the fix does not have a `source` or
  /// the metavariable is not bound in the match environment. In the
  /// latter case, the fix falls back to replacing the matched node.
  fn get_source_range<'t, D: Doc>(&self, nm: &NodeMatch<'t, D>) -> Option<Range<usize>> {
    let source = self.source.as_ref()?;
    let env = nm.get_env();
    match source {
      MetaVariable::Capture(name, _) => {
        let node = env.get_match(name)?.clone();
        Some(node.range())
      }
      MetaVariable::MultiCapture(name) => {
        let nodes = env.get_multiple_matches(name);
        // NOTE: start/end are offsets in the source's encoding, so they
        // must span nodes from the same doc to be a valid range.
        let start = nodes.first()?.range().start;
        let end = nodes.last()?.range().end;
        Some(start..end)
      }
      // Dropped/Multiple metavariables cannot identify code to replace.
      MetaVariable::Dropped(_) | MetaVariable::Multiple => None,
    }
  }
}

fn expand_start<D: Doc>(expansion: Option<&Expansion>, nm: &NodeMatch<'_, D>) -> usize {
  let node = nm.get_node();
  let mut env = std::borrow::Cow::Borrowed(nm.get_env());
  let Some(start) = expansion else {
    return node.range().start;
  };
  let node = start.stop_by.find(
    || node.prev(),
    || node.prev_all(),
    |n| start.matches.match_node_with_env(n, &mut env),
  );
  node
    .map(|n| n.range().start)
    .unwrap_or_else(|| nm.range().start)
}

fn expand_end<D: Doc>(expansion: Option<&Expansion>, nm: &NodeMatch<'_, D>) -> usize {
  let node = nm.get_node();
  let mut env = std::borrow::Cow::Borrowed(nm.get_env());
  let Some(end) = expansion else {
    return node.range().end;
  };
  let node = end.stop_by.find(
    || node.next(),
    || node.next_all(),
    |n| end.matches.match_node_with_env(n, &mut env),
  );
  node
    .map(|n| n.range().end)
    .unwrap_or_else(|| nm.range().end)
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::from_str;
  use crate::maybe::Maybe;
  use crate::test::TypeScript;
  use ast_grep_core::tree_sitter::LanguageExt;

  #[test]
  fn test_parse() {
    let fixer: SerializableFixer = from_str("test").expect("should parse");
    assert!(matches!(fixer, SerializableFixer::Str(_)));
  }

  fn parse(config: SerializableFixConfig) -> Result<Fixer, FixerError> {
    let config = SerializableFixer::Config(Box::new(config));
    let env = DeserializeEnv::new(TypeScript::Tsx);
    let fixer = Fixer::parse(&config, &env, &Some(Default::default()))?.remove(0);
    Ok(fixer)
  }

  #[test]
  fn test_deserialize_object() -> Result<(), serde_saphyr::Error> {
    let src = "{template: 'abc', expandEnd: {regex: ',', stopBy: neighbor}}";
    let SerializableFixer::Config(cfg) = from_str(src)? else {
      panic!("wrong parsing")
    };
    assert_eq!(cfg.template, "abc");
    let Maybe::Present(relation) = cfg.expand_end else {
      panic!("wrong parsing")
    };
    let rule = relation.rule;
    assert_eq!(rule.regex, Maybe::Present(",".to_string()));
    assert!(rule.pattern.is_absent());
    Ok(())
  }

  #[test]
  fn test_parse_config() -> Result<(), FixerError> {
    let relation = from_str("{regex: ',', stopBy: neighbor}").expect("should deser");
    let config = SerializableFixConfig {
      expand_end: Maybe::Present(relation),
      expand_start: Maybe::Absent,
      template: "abcd".to_string(),
      source: Maybe::Absent,
      title: None,
    };
    let ret = parse(config)?;
    assert!(ret.expand_start.is_none());
    assert!(ret.expand_end.is_some());
    assert!(matches!(ret.template, TemplateFix::Textual(_)));
    Ok(())
  }

  #[test]
  fn test_parse_str() -> Result<(), FixerError> {
    let config = SerializableFixer::Str("abcd".to_string());
    let env = DeserializeEnv::new(TypeScript::Tsx);
    let ret = Fixer::parse(&config, &env, &None)?.remove(0);
    assert!(ret.expand_end.is_none());
    assert!(ret.expand_start.is_none());
    assert!(matches!(ret.template, TemplateFix::Textual(_)));
    Ok(())
  }

  #[test]
  fn test_replace_fixer() -> Result<(), FixerError> {
    let expand_end = from_str("{regex: ',', stopBy: neighbor}").expect("should word");
    let config = SerializableFixConfig {
      expand_end: Maybe::Present(expand_end),
      expand_start: Maybe::Absent,
      template: "var $A = 456".to_string(),
      source: Maybe::Absent,
      title: None,
    };
    let fixer = parse(config)?;
    let grep = TypeScript::Tsx.ast_grep("let a = 123");
    let node = grep.root().find("let $A = 123").expect("should found");
    let edit = fixer.generate_replacement(&node);
    assert_eq!(String::from_utf8_lossy(&edit), "var a = 456");
    Ok(())
  }

  #[test]
  fn test_relace_range() -> Result<(), FixerError> {
    use ast_grep_core::matcher::KindMatcher;
    let expand_end = from_str("{regex: ',', stopBy: neighbor}").expect("should word");
    let config = SerializableFixConfig {
      expand_end: Maybe::Present(expand_end),
      expand_start: Maybe::Absent,
      template: "c: 456".to_string(),
      source: Maybe::Absent,
      title: None,
    };
    let fixer = parse(config)?;
    let grep = TypeScript::Tsx.ast_grep("var a = { b: 123, }");
    let matcher = KindMatcher::new("pair", TypeScript::Tsx);
    let node = grep.root().find(&matcher).expect("should found");
    let edit = node.make_edit(&matcher, &fixer);
    let text = String::from_utf8_lossy(&edit.inserted_text);
    assert_eq!(text, "c: 456");
    assert_eq!(edit.position, 10);
    assert_eq!(edit.deleted_length, 7);
    Ok(())
  }

  #[test]
  fn test_fixer_list() -> Result<(), FixerError> {
    let config: SerializableFixer = from_str(
      r"
- { template: 'abc', title: 'fixer 1'}
- { template: 'def', title: 'fixer 2'}",
    )
    .expect("should parse");
    let env = DeserializeEnv::new(TypeScript::Tsx);
    let fixers = Fixer::parse(&config, &env, &Some(Default::default()))?;
    assert_eq!(fixers.len(), 2);
    let config: SerializableFixer = from_str(
      r"
- { template: 'abc', title: 'fixer 1'}
- { template: 'def'}",
    )
    .expect("should parse");
    let env = DeserializeEnv::new(TypeScript::Tsx);
    let ret = Fixer::parse(&config, &env, &Some(Default::default()));
    assert!(ret.is_err());
    Ok(())
  }

  /// The fix has a `source` metavariable bound by an enclosing rule
  /// (`inside` with `stopBy: end`), so the fix should replace the node
  /// bound by the metavariable — the whole program here — instead of
  /// the matched node. See #1985.
  #[test]
  fn test_replace_source_node() -> Result<(), FixerError> {
    use crate::{RuleConfig, SerializableRuleConfig};
    let ret: Result<SerializableRuleConfig<TypeScript>, _> = from_str(
      r#"
id: test
language: Tsx
rule:
  all:
    - pattern: async($ARGS)
    - inside:
        stopBy: end
        kind: program
        pattern: $PROGRAM
fix:
  template: "import { waitForAsync } from '@angular/core/testing'; waitForAsync($ARGS)"
  source: $PROGRAM"#,
    );
    let config =
      RuleConfig::try_from(ret.expect("should deser"), &Default::default()).expect("should work");
    let grep = TypeScript::Tsx.ast_grep("import { a } from 'x'\nasync(123)\nlet b = 2");
    let node = grep.root().find(&config.matcher).expect("should found");
    let fixer = &config.fixer[0];
    let edit = node.make_edit(&config.matcher, fixer);
    assert_eq!(edit.position, 0);
    assert_eq!(
      edit.deleted_length,
      "import { a } from 'x'\nasync(123)\nlet b = 2".len()
    );
    let text = String::from_utf8_lossy(&edit.inserted_text);
    assert_eq!(
      text,
      "import { waitForAsync } from '@angular/core/testing'; waitForAsync(123)"
    );
    Ok(())
  }

  /// When the `source` metavariable is defined in the rule but not
  /// bound in the match environment, the fix falls back to replacing
  /// the matched node.
  #[test]
  fn test_unbound_source_falls_back_to_matched_node() -> Result<(), FixerError> {
    use crate::{RuleConfig, SerializableRuleConfig};
    // $UNBOUND is defined via the `not` sub-rule but never bound at
    // match time, because async(123) does not match foo($UNBOUND).
    let ret: Result<SerializableRuleConfig<TypeScript>, _> = from_str(
      r"
id: test
language: Tsx
rule:
  all:
    - pattern: async($ARGS)
    - not: { pattern: foo($UNBOUND) }
fix:
  template: 'waitFor($ARGS)'
  source: $UNBOUND",
    );
    let config =
      RuleConfig::try_from(ret.expect("should deser"), &Default::default()).expect("should work");
    let grep = TypeScript::Tsx.ast_grep("async(123)");
    let node = grep.root().find(&config.matcher).expect("should found");
    let fixer = &config.fixer[0];
    let edit = node.make_edit(&config.matcher, fixer);
    assert_eq!(edit.position, 0);
    assert_eq!(edit.deleted_length, "async(123)".len());
    let text = String::from_utf8_lossy(&edit.inserted_text);
    assert_eq!(text, "waitFor(123)");
    Ok(())
  }

  /// A `source` metavariable must be defined in `rule`/`constraints`/`transform`.
  #[test]
  fn test_fixer_source_undefined_var() -> Result<(), FixerError> {
    use crate::{RuleConfig, RuleConfigError, SerializableRuleConfig, SerializableRuleCore};
    let core: SerializableRuleCore =
      from_str("rule: {pattern: async($ARGS)}").expect("should deser");
    let fixer: SerializableFixer = from_str(
      r"
template: 'waitFor($ARGS)'
source: $UNBOUND",
    )
    .expect("should deser");
    let config = SerializableRuleConfig {
      core,
      fix: Some(fixer),
      rewriters: None,
      id: "test".into(),
      language: TypeScript::Tsx,
      message: "".into(),
      note: None,
      severity: crate::Severity::Hint,
      labels: None,
      files: None,
      ignores: None,
      url: None,
      metadata: None,
      unknown_keys: crate::rule_config::DenyUnknownKeys,
    };
    let ret = RuleConfig::try_from(config, &Default::default());
    match ret {
      Err(RuleConfigError::Core(crate::rule_core::RuleCoreError::UndefinedMetaVar(
        name,
        section,
      ))) => {
        assert_eq!(name, "UNBOUND");
        assert_eq!(section, "fix");
      }
      _ => panic!("unexpected error"),
    }
    Ok(())
  }

  #[test]
  fn test_fixer_source_malformed_var() -> Result<(), FixerError> {
    let config: SerializableFixer = from_str(
      r"
template: 'waitFor($ARGS)'
source: WRONG",
    )
    .expect("should deser");
    let SerializableFixer::Config(cfg) = &config else {
      panic!("wrong parsing")
    };
    assert_eq!(cfg.source, Maybe::Present("WRONG".to_string()));
    let env = DeserializeEnv::new(TypeScript::Tsx);
    let ret = Fixer::parse(&config, &env, &None);
    assert!(matches!(ret, Err(FixerError::InvalidSource(_))));
    Ok(())
  }
}
