use super::DeserializeEnv;
use super::RuleSerializeError;
use crate::plugin::{Plugin, PluginNode};

use ast_grep_core::language::Language;
use ast_grep_core::meta_var::MetaVarEnv;
use ast_grep_core::{Doc, Matcher, Node};

use bit_set::BitSet;

use std::borrow::Cow;
use std::sync::Arc;

/// A rule that delegates matching to a registered [`Plugin`].
pub struct PluginMatcher {
  plugin: Arc<dyn Plugin>,
}

impl PluginMatcher {
  pub(crate) fn try_new<L: Language>(
    name: &str,
    env: &DeserializeEnv<L>,
  ) -> Result<Self, RuleSerializeError> {
    let plugin = env
      .get_plugin(name)
      .ok_or_else(|| RuleSerializeError::UndefinedPlugin(name.to_string()))?
      .clone();
    Ok(Self { plugin })
  }
}

impl Matcher for PluginMatcher {
  fn match_node_with_env<'tree, D: Doc>(
    &self,
    node: Node<'tree, D>,
    _env: &mut Cow<MetaVarEnv<'tree, D>>,
  ) -> Option<Node<'tree, D>> {
    let kind = node.kind();
    let text = node.text();
    let input = PluginNode {
      kind: &kind,
      text: &text,
      range: node.range(),
      lines: node.start_pos().line()..node.end_pos().line(),
    };
    self.plugin.is_match(&input).then_some(node)
  }

  fn potential_kinds(&self) -> Option<BitSet> {
    // A plugin can match any node, so ast-grep cannot narrow the scan by kind.
    // Combine `plugin` with `kind` or `pattern` to give the scan a kind set.
    None
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::test::TypeScript;
  use crate::{GlobalRules, SerializableRule, from_str};
  use ast_grep_core::tree_sitter::LanguageExt;

  struct TextStartsWith(&'static str);

  impl Plugin for TextStartsWith {
    fn is_match(&self, node: &PluginNode<'_>) -> bool {
      node.text.starts_with(self.0)
    }
  }

  struct IsKind(&'static str);

  impl Plugin for IsKind {
    fn is_match(&self, node: &PluginNode<'_>) -> bool {
      node.kind == self.0
    }
  }

  fn globals() -> GlobalRules {
    GlobalRules::default()
      .with_plugin("starts-with-foo", Arc::new(TextStartsWith("foo")))
      .with_plugin("is-identifier", Arc::new(IsKind("identifier")))
  }

  fn deserialize(yaml: &str, globals: &GlobalRules) -> Result<Rule, RuleSerializeError> {
    let rule: SerializableRule = from_str(yaml).expect("should parse rule");
    let env = DeserializeEnv::new(TypeScript::Tsx).with_globals(globals);
    env.deserialize_rule(rule)
  }

  use crate::rule::Rule;

  #[test]
  fn test_plugin_matches_node() {
    let rule = deserialize(
      r"
all:
  - kind: identifier
  - plugin: starts-with-foo
",
      &globals(),
    )
    .expect("plugin should deserialize");
    let grep = TypeScript::Tsx.ast_grep("foo = bar");
    assert!(grep.root().find(&rule).is_some());
    let grep = TypeScript::Tsx.ast_grep("baz = bar");
    assert!(grep.root().find(&rule).is_none());
  }

  #[test]
  fn test_plugin_sees_node_data() {
    let rule = deserialize("plugin: is-identifier", &globals()).expect("should deserialize");
    let grep = TypeScript::Tsx.ast_grep("const a = 123");
    assert!(grep.root().find(&rule).is_some());
    let grep = TypeScript::Tsx.ast_grep("123");
    assert!(grep.root().find(&rule).is_none());
  }

  #[test]
  fn test_plugin_in_constraint() {
    let globals = globals();
    let rule: crate::SerializableRuleCore = from_str(
      r"
rule:
  pattern: $CALLEE($$$ARGS)
constraints:
  CALLEE:
    all:
      - kind: identifier
      - plugin: starts-with-foo
",
    )
    .expect("should parse");
    let env = DeserializeEnv::new(TypeScript::Tsx).with_globals(&globals);
    let matcher = rule.get_matcher(env).expect("should deserialize");
    let grep = TypeScript::Tsx.ast_grep("fooBar(1)");
    assert!(grep.root().find(&matcher).is_some());
    let grep = TypeScript::Tsx.ast_grep("barFoo(1)");
    assert!(grep.root().find(&matcher).is_none());
  }

  #[test]
  fn test_undefined_plugin() {
    let err = match deserialize("plugin: not-registered", &globals()) {
      Err(e) => e,
      Ok(_) => panic!("should be an error"),
    };
    assert!(matches!(err, RuleSerializeError::UndefinedPlugin(name) if name == "not-registered"));
  }

  #[test]
  fn test_plugin_without_registration() {
    let err = match deserialize("plugin: starts-with-foo", &GlobalRules::default()) {
      Err(e) => e,
      Ok(_) => panic!("should be an error"),
    };
    assert!(matches!(err, RuleSerializeError::UndefinedPlugin(_)));
  }
}
