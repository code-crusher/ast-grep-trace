//! An interactive REPL to debug why a rule does or does not match.
//!
//! `sg run --debug` and `sg scan --debug` pause at every AST node that can
//! potentially match the rule(s) and drop into a simple line based console.
//! The console can print the current node, the rule under test, the matched
//! metavariables and the node's source code, and it can resume the traversal
//! at a different position.
//!
//! The console reads commands line by line from stdin instead of raw key
//! events. This keeps the debugger usable when stdin is redirected and makes
//! the command handling testable without a terminal.

use crate::lang::SgLang;
use crate::utils::debug_query::dump_node;

use anyhow::Result;
use ast_grep_core::matcher::MatcherExt;
use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_core::{Matcher, Node};

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::ops::Range;
use std::path::Path;

type AstGrep = ast_grep_core::AstGrep<StrDoc<SgLang>>;
type SgNode<'t> = Node<'t, StrDoc<SgLang>>;

/// A rule exposed to the debugger.
pub struct DebugRule<'a, M: Matcher> {
  /// A human readable name, e.g. the rule id or `pattern`.
  pub label: String,
  /// The rule description printed by the `print-rule` command.
  pub description: String,
  /// The matcher used to test candidate nodes.
  pub matcher: &'a M,
}

enum Flow {
  /// Keep the session alive at the current node.
  Stay,
  /// Resume traversal and do not pause any more.
  Finish,
  /// Stop the whole debugging session.
  Quit,
}

struct Debugger<'a, 't, M: Matcher> {
  colored: bool,
  rules: &'a [DebugRule<'a, M>],
  /// `potential_kinds` of every rule, cached as kind id lists.
  filters: Vec<Option<Vec<usize>>>,
  /// All nodes that can possibly match at least one rule, in pre-order.
  candidates: Vec<SgNode<'t>>,
  /// Index of the current node in `candidates`.
  pos: usize,
  /// Position of the current rule in the rules attempted at the current node.
  rule_pos: usize,
  /// Whether the next prompt should print the pause banner.
  banner: bool,
}

fn kind_filters<M: Matcher>(rules: &[DebugRule<M>]) -> Vec<Option<Vec<usize>>> {
  rules
    .iter()
    .map(|rule| {
      rule
        .matcher
        .potential_kinds()
        .map(|kinds| kinds.iter().collect())
    })
    .collect()
}

fn collect_candidates<'t>(root: &SgNode<'t>, filters: &[Option<Vec<usize>>]) -> Vec<SgNode<'t>> {
  if filters.is_empty() {
    return vec![];
  }
  // a rule without potential kinds can match any node
  let match_all = filters.iter().any(Option::is_none);
  root
    .dfs()
    .filter(|node| {
      let kind = node.kind_id() as usize;
      match_all
        || filters
          .iter()
          .flatten()
          .any(|kinds| kinds.contains(&kind))
    })
    .collect()
}

impl<'a, 't, M: Matcher> Debugger<'a, 't, M> {
  fn node(&self) -> &SgNode<'t> {
    &self.candidates[self.pos]
  }

  /// Indices into `rules` of the rules attempted at the current node.
  fn attempted(&self) -> Vec<usize> {
    let kind = self.node().kind_id() as usize;
    self
      .rules
      .iter()
      .enumerate()
      .filter(|(idx, _)| match &self.filters[*idx] {
        Some(kinds) => kinds.contains(&kind),
        None => true,
      })
      .map(|(idx, _)| idx)
      .collect()
  }

  fn current_rule(&self) -> (usize, &'a DebugRule<'a, M>) {
    let attempted = self.attempted();
    if attempted.is_empty() {
      // only reachable if a session is started without any rule
      return (0, &self.rules[0]);
    }
    let pos = self.rule_pos.min(attempted.len() - 1);
    (pos, &self.rules[attempted[pos]])
  }

  /// Move to the next candidate node outside of `range` (the current subtree).
  fn next_after(&self, range: Range<usize>) -> Option<usize> {
    (self.pos + 1..self.candidates.len()).find(|&idx| {
      let candidate = self.candidates[idx].range();
      !(range.start <= candidate.start && candidate.end <= range.end)
    })
  }

  fn goto(&mut self, pos: usize) {
    self.pos = pos;
    self.rule_pos = 0;
    self.banner = true;
  }

  fn step_after_subtree(&mut self, out: &mut impl Write) -> Result<Flow> {
    let range = self.node().range();
    match self.next_after(range) {
      Some(pos) => self.goto(pos),
      None => {
        writeln!(out, "no more candidate nodes")?;
        return Ok(Flow::Finish);
      }
    }
    Ok(Flow::Stay)
  }

  fn step_into(&mut self, out: &mut impl Write) -> Result<Flow> {
    let range = self.node().range();
    let next = self.candidates.get(self.pos + 1).map(|node| node.range());
    match next {
      Some(next) if range.start <= next.start && next.end <= range.end => {
        self.goto(self.pos + 1);
        Ok(Flow::Stay)
      }
      // no candidate inside this node, so `into` falls back to skipping it
      _ => self.step_after_subtree(out),
    }
  }

  fn step_out(&mut self, out: &mut impl Write) -> Result<Flow> {
    let Some(parent) = self.node().parent() else {
      writeln!(out, "no more candidate nodes")?;
      return Ok(Flow::Finish);
    };
    match self.next_after(parent.range()) {
      Some(pos) => self.goto(pos),
      None => {
        writeln!(out, "no more candidate nodes")?;
        return Ok(Flow::Finish);
      }
    }
    Ok(Flow::Stay)
  }

  fn move_rule(&mut self, down: bool, out: &mut impl Write) -> Result<()> {
    let attempted = self.attempted();
    if attempted.len() < 2 {
      writeln!(
        out,
        "only one rule is attempted at this node; nothing to move to"
      )?;
      return Ok(());
    }
    let pos = self.rule_pos.min(attempted.len() - 1);
    self.rule_pos = if down {
      (pos + 1) % attempted.len()
    } else {
      (pos + attempted.len() - 1) % attempted.len()
    };
    let (_, rule) = self.current_rule();
    writeln!(out, "-- rule `{}`\n{}", rule.label, rule.description)?;
    Ok(())
  }

  fn print_metavars(&self, out: &mut impl Write) -> Result<()> {
    let (_, rule) = self.current_rule();
    let Some(matched) = rule.matcher.match_node(self.node().clone()) else {
      writeln!(
        out,
        "rule `{}` does not match the current node",
        rule.label
      )?;
      return Ok(());
    };
    let mut vars: Vec<(String, String)> = HashMap::from(matched.get_env().clone())
      .into_iter()
      .collect();
    vars.sort();
    if vars.is_empty() {
      writeln!(out, "no metavariables captured")?;
    }
    for (name, value) in vars {
      writeln!(out, "${name} = {value}")?;
    }
    Ok(())
  }

  fn print_pause(&self, out: &mut impl Write) -> Result<()> {
    let node = self.node();
    let (start_line, start_col) = node.start_pos().byte_point();
    let (end_line, end_col) = node.end_pos().byte_point();
    writeln!(
      out,
      "-- node: {} ({start_line},{start_col})-({end_line},{end_col})",
      node.kind()
    )?;
    let attempted = self.attempted();
    let current = self.rule_pos.min(attempted.len().saturating_sub(1));
    for (pos, idx) in attempted.iter().enumerate() {
      let rule = &self.rules[*idx];
      let matched = rule.matcher.match_node(node.clone()).is_some();
      let marker = if pos == current { "*" } else { " " };
      let status = if matched { "matched" } else { "no match" };
      writeln!(out, "   [{marker}] {}: {status}", rule.label)?;
    }
    Ok(())
  }

  fn handle(&mut self, command: &str, out: &mut impl Write) -> Result<Flow> {
    let command = command.trim();
    match command {
      "" | "n" | "next" => self.step_after_subtree(out),
      "i" | "into" => self.step_into(out),
      "o" | "out" => self.step_out(out),
      "c" | "continue" => Ok(Flow::Finish),
      "q" | "quit" | "exit" => Ok(Flow::Quit),
      "j" => {
        if self.pos + 1 < self.candidates.len() {
          self.goto(self.pos + 1);
        } else {
          writeln!(out, "already at the last candidate node")?;
        }
        Ok(Flow::Stay)
      }
      "k" => {
        if self.pos > 0 {
          self.goto(self.pos - 1);
        } else {
          writeln!(out, "already at the first candidate node")?;
        }
        Ok(Flow::Stay)
      }
      "u" | "up" => self.move_rule(false, out).map(|()| Flow::Stay),
      "d" | "down" => self.move_rule(true, out).map(|()| Flow::Stay),
      "m" | "metavars" => self.print_metavars(out).map(|()| Flow::Stay),
      "pn" | "print-node" | "node" => {
        let ast = dump_node(self.node().get_inner_node()).ast(self.colored);
        writeln!(out, "{ast}")?;
        Ok(Flow::Stay)
      }
      "pr" | "print-rule" | "rule" => {
        let (_, rule) = self.current_rule();
        writeln!(out, "# rule `{}`\n{}", rule.label, rule.description)?;
        Ok(Flow::Stay)
      }
      "s" | "source" => {
        writeln!(out, "{}", self.node().text())?;
        Ok(Flow::Stay)
      }
      "h" | "help" | "?" => {
        print_help(out)?;
        Ok(Flow::Stay)
      }
      _ => {
        writeln!(out, "unknown command `{command}`. Type `h` for help.")?;
        Ok(Flow::Stay)
      }
    }
  }
}

fn print_help(out: &mut impl Write) -> Result<()> {
  writeln!(
    out,
    "\
Commands:
  m, metavars      print metavariables captured by the current rule
  pn, print-node   print the current node as an AST
  pr, print-rule   print the rule being attempted
  s, source        print the source code of the current node
  u, up            move to the previous rule attempted at this node
  d, down          move to the next rule attempted at this node
  k                move to the previous candidate node
  j                move to the next candidate node
  n, next          resume at the next sibling, skipping this node's children
  i, into          resume into this node's children
  o, out           resume after this node's parent, skipping remaining siblings
  c, continue      resume and run to the end without pausing
  q, quit          quit the debugger
  h, help          print this help
  <enter>          same as next"
  )?;
  Ok(())
}

/// The outcome of one debugging session.
pub struct DebugOutcome {
  /// Whether the user asked to quit, so callers should stop processing files.
  pub quit: bool,
  /// How many candidate nodes the file contained.
  pub candidates: usize,
}

/// Run an interactive debugging session over `grep`.
pub fn debug_session<M: Matcher, R: BufRead, W: Write>(
  grep: &AstGrep,
  path: &Path,
  rules: &[DebugRule<'_, M>],
  colored: bool,
  input: &mut R,
  output: &mut W,
) -> Result<DebugOutcome> {
  let filters = kind_filters(rules);
  let candidates = collect_candidates(&grep.root(), &filters);
  if candidates.is_empty() {
    // stay quiet per file; callers report when no file has a candidate at all
    return Ok(DebugOutcome {
      quit: false,
      candidates: 0,
    });
  }
  let candidate_count = candidates.len();
  writeln!(
    output,
    "Debugging {} with {} rule(s). Type `h` for help.",
    path.display(),
    rules.len()
  )?;
  let mut debugger = Debugger {
    colored,
    rules,
    filters,
    candidates,
    pos: 0,
    rule_pos: 0,
    banner: true,
  };
  loop {
    if debugger.banner {
      debugger.print_pause(output)?;
      debugger.banner = false;
    }
    write!(output, "debug> ")?;
    output.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
      // stdin is closed, behave as if the user typed `continue`
      break;
    }
    match debugger.handle(&line, output)? {
      Flow::Stay => {}
      Flow::Finish => break,
      Flow::Quit => {
        return Ok(DebugOutcome {
          quit: true,
          candidates: candidate_count,
        });
      }
    }
  }
  Ok(DebugOutcome {
    quit: false,
    candidates: candidate_count,
  })
}

/// Run a debugging session with the process' standard input and output.
pub fn run_debug_session<M: Matcher>(
  grep: &AstGrep,
  path: &Path,
  rules: &[DebugRule<'_, M>],
  colored: bool,
) -> Result<DebugOutcome> {
  let stdin = std::io::stdin();
  let mut input = stdin.lock();
  let stdout = std::io::stdout();
  let mut output = stdout.lock();
  debug_session(grep, path, rules, colored, &mut input, &mut output)
}

#[cfg(test)]
mod test {
  use super::*;
  use ast_grep_core::Pattern;
  use ast_grep_language::{LanguageExt, SupportLang};

  fn run_session(input: &str, colored: bool) -> (String, bool) {
    let lang: SgLang = SupportLang::TypeScript.into();
    let grep = lang.ast_grep("foo(1); bar(2); foo(3)");
    let rule = Pattern::new("foo($A)", lang);
    let rules = [DebugRule {
      label: "test-rule".to_string(),
      description: "pattern: foo($A)".to_string(),
      matcher: &rule,
    }];
    let mut output = Vec::new();
    let outcome = debug_session(
      &grep,
      Path::new("test.ts"),
      &rules,
      colored,
      &mut input.as_bytes(),
      &mut output,
    )
    .expect("session should not fail");
    (String::from_utf8(output).expect("valid utf8"), outcome.quit)
  }

  #[test]
  fn test_pause_on_candidates_only() {
    let (output, quit) = run_session("c\n", false);
    assert!(!quit);
    assert!(output.contains("Debugging test.ts with 1 rule(s)"), "{output}");
    assert!(output.contains("-- node: call_expression"), "{output}");
    assert!(output.contains("[*] test-rule: matched"), "{output}");
  }

  #[test]
  fn test_print_node_metavars_source_and_rule() {
    let (output, _) = run_session("pn\nm\npr\ns\nc\n", false);
    assert!(output.contains("call_expression"), "{output}");
    assert!(output.contains("$A = 1"), "{output}");
    assert!(output.contains("# rule `test-rule`"), "{output}");
    assert!(output.contains("pattern: foo($A)"), "{output}");
    assert!(output.contains("foo(1)"), "{output}");
  }

  #[test]
  fn test_metavars_when_not_matched() {
    // second candidate is `bar(2)`, which does not match foo($A)
    let (output, _) = run_session("n\nm\nc\n", false);
    assert!(
      output.contains("rule `test-rule` does not match the current node"),
      "{output}"
    );
  }

  #[test]
  fn test_into_next_and_out_navigation() {
    // MatchAll matches any node, so every AST node is a candidate.
    let lang: SgLang = SupportLang::TypeScript.into();
    let grep = lang.ast_grep("a; b;");
    let rule = ast_grep_core::matcher::MatchAll;
    let rules = [DebugRule {
      label: "any".to_string(),
      description: "kind: *".to_string(),
      matcher: &rule,
    }];
    let mut output = Vec::new();
    debug_session(
      &grep,
      Path::new("test.ts"),
      &rules,
      false,
      &mut "i\nn\no\n".as_bytes(),
      &mut output,
    )
    .expect("session should not fail");
    let output = String::from_utf8(output).expect("valid utf8");
    let kinds: Vec<_> = output
      .lines()
      .filter_map(|line| line.split_once("-- node: ").map(|(_, rest)| rest))
      .map(|rest| rest.split_whitespace().next().unwrap_or(rest))
      .collect();
    // program -> first statement -> (skip its subtree) second statement
    assert_eq!(
      kinds,
      ["program", "expression_statement", "expression_statement"]
    );
    assert!(output.contains("no more candidate nodes"), "{output}");
  }

  #[test]
  fn test_j_k_navigation() {
    let (output, _) = run_session("j\nk\nk\nc\n", false);
    assert!(
      output.contains("already at the first candidate node"),
      "{output}"
    );
    assert!(!output.contains("no more candidate nodes"), "{output}");
  }

  #[test]
  fn test_quit() {
    let (output, quit) = run_session("q\n", false);
    assert!(quit, "{output}");
  }

  #[test]
  fn test_help_and_unknown_command() {
    let (output, _) = run_session("h\nnope\nc\n", false);
    assert!(output.contains("Commands:"), "{output}");
    assert!(output.contains("unknown command `nope`"), "{output}");
  }

  #[test]
  fn test_eof_finishes_session() {
    let (output, quit) = run_session("", false);
    assert!(!quit);
    assert!(output.contains("-- node: call_expression"), "{output}");
  }

  #[test]
  fn test_multiple_rules_move_rule_frame() {
    let lang: SgLang = SupportLang::TypeScript.into();
    let grep = lang.ast_grep("foo(1)");
    let first = Pattern::new("foo($A)", lang);
    let second = Pattern::new("foo($B)", lang);
    let rules = [
      DebugRule {
        label: "first".to_string(),
        description: "pattern: foo($A)".to_string(),
        matcher: &first,
      },
      DebugRule {
        label: "second".to_string(),
        description: "pattern: foo($B)".to_string(),
        matcher: &second,
      },
    ];
    let mut output = Vec::new();
    debug_session(
      &grep,
      Path::new("test.ts"),
      &rules,
      false,
      &mut "d\nu\nc\n".as_bytes(),
      &mut output,
    )
    .expect("session should not fail");
    let output = String::from_utf8(output).expect("valid utf8");
    assert!(output.contains("[*] first: matched"), "{output}");
    assert!(output.contains("[ ] second: matched"), "{output}");
    assert!(output.contains("-- rule `second`"), "{output}");
    assert!(output.contains("-- rule `first`"), "{output}");
  }
}
