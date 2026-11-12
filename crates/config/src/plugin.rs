//! Plugin matchers let users extend ast-grep's matching beyond the built-in rule language.
//!
//! A plugin is a custom matcher implemented outside of ast-grep. It is registered
//! under a name and referenced from a rule with the `plugin` key:
//!
//! ```yaml
//! rule:
//!   all:
//!     - kind: identifier
//!     - plugin: my-plugin
//! ```
//!
//! Libraries embedding ast-grep can register in-process plugins with
//! [`GlobalRules::with_plugin`](crate::GlobalRules::with_plugin). The CLI loads
//! native plugins configured in `sgconfig.yml`; see the `ast-grep-dynamic` crate.

use serde::Serialize;

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

/// Data about a candidate AST node passed to a [`Plugin`].
///
/// Plugins should not assume any particular language. `kind` is the
/// tree-sitter kind name and `range` is relative to the beginning of the
/// document, which lets a plugin consult its own index or symbol table.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginNode<'a> {
  /// The tree-sitter kind name of the node, e.g. `identifier`.
  pub kind: &'a str,
  /// The exact source text of the node.
  pub text: &'a str,
  /// Range of the node in the document's source units.
  pub range: Range<usize>,
  /// Zero-based line range of the node in the document.
  pub lines: Range<usize>,
}

/// A custom matcher implemented outside ast-grep's built-in rule language.
///
/// A plugin only decides whether a node matches. It cannot define meta
/// variables or change the matched node, so it composes with other rules like
/// an atomic rule. Implementations must be thread safe because scanning is
/// parallel.
pub trait Plugin: Send + Sync {
  /// Returns `true` if the node should be considered a match.
  fn is_match(&self, node: &PluginNode<'_>) -> bool;
}

/// A name to [`Plugin`] map shared by every rule in a project.
#[derive(Clone, Default)]
pub struct PluginRegistry {
  plugins: Arc<HashMap<String, Arc<dyn Plugin>>>,
}

impl PluginRegistry {
  pub fn new() -> Self {
    Self::default()
  }

  /// Register `plugin` under `name`, replacing any previous plugin with the same name.
  pub fn register(
    &mut self,
    name: impl Into<String>,
    plugin: Arc<dyn Plugin>,
  ) -> Option<Arc<dyn Plugin>> {
    Arc::make_mut(&mut self.plugins).insert(name.into(), plugin)
  }

  pub fn get(&self, name: &str) -> Option<&Arc<dyn Plugin>> {
    self.plugins.get(name)
  }

  pub fn is_empty(&self) -> bool {
    self.plugins.is_empty()
  }

  pub fn len(&self) -> usize {
    self.plugins.len()
  }
}

#[cfg(test)]
mod test {
  use super::*;

  struct Always;

  impl Plugin for Always {
    fn is_match(&self, _node: &PluginNode<'_>) -> bool {
      true
    }
  }

  #[test]
  fn test_registry() {
    let mut registry = PluginRegistry::new();
    assert!(registry.is_empty());
    assert!(registry.get("test").is_none());
    registry.register("test", Arc::new(Always));
    assert_eq!(registry.len(), 1);
    assert!(registry.get("test").is_some());
  }
}
