use super::Matcher;
use ast_grep_core::meta_var::MetaVarEnv;
use ast_grep_core::{Doc, Node};
use std::borrow::Cow;

use bit_set::BitSet;
use regex::{Error as RegexError, Regex};
use thiserror::Error;

/// Errors that can occur when creating or using a PathMatcher
#[derive(Debug, Error)]
pub enum PathMatcherError {
  /// The path regex failed to compile.
  #[error("Parsing path matcher fails.")]
  Regex(#[from] RegexError),
}

/// Match a node when the path of the file being linted matches the given regex.
///
/// The regular expression is matched against the file path as it was passed to
/// ast-grep (or as normalized relative to the project directory when scanning a
/// project). When the document does not correspond to a file on disk
/// (e.g. stdin, language servers or in-memory test documents), the matcher
/// never matches.
///
/// The matcher itself has no knowledge of AST kinds, so, just like the `regex`
/// atomic rule, it must be combined with a `kind` or `pattern` rule via `all`.
#[derive(Clone)]
pub struct PathMatcher {
  regex: Regex,
}

impl PathMatcher {
  pub fn try_new(path: &str) -> Result<Self, PathMatcherError> {
    Ok(Self {
      regex: Regex::new(path)?,
    })
  }
}

impl Matcher for PathMatcher {
  fn match_node_with_env<'tree, D: Doc>(
    &self,
    node: Node<'tree, D>,
    _env: &mut Cow<MetaVarEnv<'tree, D>>,
  ) -> Option<Node<'tree, D>> {
    let path = node.get_doc().path()?;
    // Always match against a forward-slash separated path so the rule
    // behaves the same across operating systems.
    let normalized = path.to_string_lossy().replace('\\', "/");
    self.regex.is_match(&normalized).then_some(node)
  }

  fn potential_kinds(&self) -> Option<BitSet> {
    None
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::test::TypeScript;
  use ast_grep_core::matcher::MatcherExt;
  use ast_grep_core::tree_sitter::{LanguageExt, Root};

  fn doc_with_path(
    path: &str,
    source: &str,
  ) -> Root<ast_grep_core::tree_sitter::StrDoc<TypeScript>> {
    Root::try_new_with_path(source, TypeScript::Tsx, Some(path.into())).expect("should parse")
  }

  #[test]
  fn test_path_match() {
    let matcher = PathMatcher::try_new(r"use-cases/.*\.use-case\.ts$").unwrap();
    let root = doc_with_path(
      "src/application/use-cases/create-user.use-case.ts",
      "let a = 1",
    );
    assert!(matcher.find_node(root.root()).is_some());
  }

  #[test]
  fn test_path_no_match() {
    let matcher = PathMatcher::try_new(r"use-cases/.*\.use-case\.ts$").unwrap();
    let root = doc_with_path("src/application/services/create-user.ts", "let a = 1");
    assert!(matcher.find_node(root.root()).is_none());
  }

  #[test]
  fn test_path_backslashes_are_normalized() {
    let matcher = PathMatcher::try_new(r"use-cases/create-user\.use-case\.ts$").unwrap();
    let root = doc_with_path(
      "src\\application\\use-cases\\create-user.use-case.ts",
      "let a = 1",
    );
    assert!(matcher.find_node(root.root()).is_some());
  }

  #[test]
  fn test_path_absent() {
    let matcher = PathMatcher::try_new(r".*").unwrap();
    let root = TypeScript::Tsx.ast_grep("let a = 1");
    assert!(matcher.find_node(root.root()).is_none());
  }
}
