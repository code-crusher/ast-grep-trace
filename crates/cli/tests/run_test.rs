mod common;

use anyhow::Result;
use assert_cmd::{Command, cargo_bin};
use common::create_test_files;
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::Value;

#[test]
fn test_simple_infer_lang() -> Result<()> {
  let dir = create_test_files([("a.ts", "console.log(123)"), ("b.rs", "console.log(456)")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "console.log($A)"])
    .assert()
    .success()
    .stdout(contains("console.log(123)"))
    .stdout(contains("console.log(456)"));
  Ok(())
}

#[test]
fn test_simple_specific_lang() -> Result<()> {
  let dir = create_test_files([("a.ts", "console.log(123)"), ("b.rs", "console.log(456)")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "console.log($A)", "-l", "rs"])
    .assert()
    .success()
    .stdout(contains("console.log(123)").not())
    .stdout(contains("console.log(456)"));
  Ok(())
}

#[test]
fn test_kind_selector() -> Result<()> {
  let dir = create_test_files([("a.js", "test(123)\nconst test = 456")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["run", "-k", "call_expression > identifier", "-l", "js"])
    .assert()
    .success()
    .stdout(contains("test(123)"))
    .stdout(contains("const test = 456").not());
  Ok(())
}

#[test]
fn test_default_run_with_kind_selector() -> Result<()> {
  let dir = create_test_files([("a.js", "test(123)\nconst test = 456")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-k", "call_expression > identifier", "-l", "js"])
    .assert()
    .success()
    .stdout(contains("test(123)"))
    .stdout(contains("const test = 456").not());
  Ok(())
}

#[test]
fn test_kind_selector_error_context() -> Result<()> {
  let dir = create_test_files([("a.js", "test(123)")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["run", "-k", "call_expression >", "-l", "js"])
    .assert()
    .failure()
    .stderr(contains("Cannot parse kind as a valid selector."));
  Ok(())
}

#[test]
fn test_js_in_html() -> Result<()> {
  let dir = create_test_files([
    ("a.html", "<script>alert(1)</script>"),
    ("b.js", "alert(456)"),
  ])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "alert($A)", "-l", "js"])
    .assert()
    .success()
    .stdout(contains("alert(1)"))
    .stdout(contains("alert(456)"));
  Ok(())
}

#[test]
fn test_outline_javascript_in_vue_as_html() -> Result<()> {
  let dir = create_test_files([
    (
      "sgconfig.yml",
      r#"ruleDirs: []
languageGlobs:
  html:
    - "*.vue"
"#,
    ),
    (
      "component.vue",
      r#"<template><main>Hello</main></template>
<script lang="typescript">
export function greet(name: string) {
  return `Hello ${name}`;
}
</script>"#,
    ),
  ])?;

  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["outline", "component.vue", "--json=compact"])
    .assert()
    .success()
    .stdout(contains(r#""language":"Html""#))
    .stdout(contains(r#""name":"greet""#));
  Ok(())
}

/// Verifies that outline honors project-configured hidden-file searches.
#[test]
fn test_outline_no_ignore_from_config() -> Result<()> {
  let dir = create_test_files([
    ("sgconfig.yml", "ruleDirs: []\nnoIgnore:\n  - hidden\n"),
    (".hidden.js", "export function hiddenFunction() {}"),
  ])?;

  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["outline", ".", "--json=compact"])
    .assert()
    .success()
    .stdout(contains(r#""name":"hiddenFunction""#));
  Ok(())
}

#[test]
fn test_outline_javascript_in_html_stdin() -> Result<()> {
  Command::new(cargo_bin!())
    .args(["outline", "--stdin", "--lang", "html", "--json=compact"])
    .write_stdin(
      r#"<script lang="typescript">
export function greet(name: string) {
  return `Hello ${name}`;
}
</script>"#,
    )
    .assert()
    .success()
    .stdout(contains(r#""path":"STDIN""#))
    .stdout(contains(r#""language":"Html""#))
    .stdout(contains(r#""name":"greet""#));
  Ok(())
}

#[test]
fn test_outline_markdown_headings() -> Result<()> {
  let dir = create_test_files([(
    "guide.md",
    r#"# Introduction

Usage
=====

###### Details

References
----------

```markdown
# Not a heading
```
"#,
  )])?;

  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["outline", "guide.md", "--json=compact"])
    .assert()
    .success()
    .stdout(contains(r#""language":"Markdown""#))
    .stdout(contains(r#""name":"Introduction""#))
    .stdout(contains(r#""name":"Usage""#))
    .stdout(contains(r#""name":"Details""#))
    .stdout(contains(r#""name":"References""#))
    .stdout(contains(r#""name":"Not a heading""#).not());
  Ok(())
}

#[test]
fn test_rewrite_js_in_html() -> Result<()> {
  let dir = create_test_files([("a.html", "<script>alert(1)</script>")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "alert($A)", "-r", "alert(456)"])
    .assert()
    .success()
    .stdout(contains("alert(1)"))
    .stdout(contains("alert(456)"));
  Ok(())
}

#[test]
fn test_inspect() -> Result<()> {
  let dir = create_test_files([("a.js", "alert(1)"), ("b.js", "alert(456)")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "alert($A)", "-l", "js", "--inspect", "entity"])
    .assert()
    .success()
    .stdout(contains("alert(1)"))
    .stderr(contains("scannedFileCount=2"));
  Ok(())
}

#[test]
fn test_inspect_skipped_file() -> Result<()> {
  let dir = create_test_files([("a.js", "alert(1)"), ("empty.js", "")])?;
  // GBK-encoded comment, not valid UTF-8
  std::fs::write(dir.path().join("gbk.js"), b"alert(2) // \xd6\xd0\xce\xc4")?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "alert($A)", "-l", "js", "--inspect", "entity"])
    .assert()
    .success()
    .stderr(contains("sg: entity|file|gbk.js: skipped=invalidUtf8"))
    .stderr(contains("sg: entity|file|empty.js: skipped=emptyFile"))
    .stderr(contains("sg: entity|file|gbk.js: language").not())
    .stderr(contains("scannedFileCount=3,skippedFileCount=2"));
  Ok(())
}

#[test]
fn test_status_code_fail_with_no_match() -> Result<()> {
  let dir = create_test_files([("a.js", "alert(1)")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "no-match"])
    .assert()
    .failure()
    .stdout(predicate::str::is_empty());
  Ok(())
}

#[test]
fn test_debug_query() -> Result<()> {
  // should not print pattern if invalid
  Command::new(cargo_bin!())
    .args(["-p", "foo;bar;", "-l", "js", "--debug-query"])
    .assert()
    .failure()
    .stderr(contains("Debug Pattern").not())
    .stderr(contains("Cannot parse query as a valid pattern"));

  // should  print debug tree even for invalid pattern
  Command::new(cargo_bin!())
    .args(["-p", "foo;bar;", "-l", "js", "--debug-query=ast"])
    .assert()
    .failure()
    .stderr(contains("Debug AST"))
    .stderr(contains("Cannot parse query as a valid pattern"));

  Ok(())
}

#[test]
fn test_config_arg_in_default_run() -> Result<()> {
  let dir = create_test_files([("sgconfig.yml", "ruleDirs: []"), ("test.js", "alert(123)")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "alert($A)", "-c", "sgconfig.yml", "test.js"])
    .assert()
    .success()
    .stdout(contains("alert(123)"));
  Ok(())
}

/// Verifies that run honors project-configured hidden-file searches.
#[test]
fn test_no_ignore_from_config() -> Result<()> {
  let dir = create_test_files([
    ("sgconfig.yml", "ruleDirs: []\nnoIgnore:\n  - hidden\n"),
    (".hidden.js", "alert(123)"),
  ])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "alert($A)", "-l", "js"])
    .assert()
    .success()
    .stdout(contains("alert(123)"));
  Ok(())
}

#[test]
fn test_cli_no_ignore_overrides_config() -> Result<()> {
  let dir = create_test_files([
    ("sgconfig.yml", "ruleDirs: []\nnoIgnore: [hidden]\n"),
    (".hidden/a.js", "alert(123)"),
    ("visible.js", "alert(456)"),
  ])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "alert($A)", "-l", "js", "--no-ignore", "vcs"])
    .assert()
    .success()
    .stdout(contains("alert(456)"))
    .stdout(contains("alert(123)").not());
  Ok(())
}

#[test]
fn test_trace_default_project() -> Result<()> {
  let dir = create_test_files([("sgconfig.yml", "ruleDirs: []")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "alert($A)", "--inspect=summary"])
    .assert()
    .failure()
    .stderr(contains("isProject=true,projectDir"));
  Ok(())
}

#[test]
fn test_trace_project() -> Result<()> {
  let dir = create_test_files([("not.yml", "ruleDirs: []")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["-p", "alert($A)", "--inspect=summary"])
    .assert()
    .failure()
    .stderr(contains("isProject=false"));
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["run", "-c=not.yml", "-p", "alert($A)", "--inspect=summary"])
    .assert()
    .failure()
    .stderr(contains("isProject=true,projectDir"));
  Ok(())
}

/// The ripgrep JSON format must be a drop-in event stream: a `begin` event per
/// matching file, coalesced `match` events with submatches, retained `context`
/// lines from -B/-A, an `end` event and a final `summary`. See issue #768.
#[test]
fn test_json_ripgrep_format() -> Result<()> {
  let dir = create_test_files([("b.rs", "aaa\nbbb bbb bbb\nccc\n")])?;
  let output = Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["--json=ripgrep", "-B1", "-p", "bbb", "."])
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
  let events: Vec<Value> = String::from_utf8(output)?
    .lines()
    .map(|l| serde_json::from_str(l).expect("each line is one JSON event"))
    .collect();
  let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
  assert_eq!(types, ["begin", "context", "match", "end", "summary"]);

  let begin = &events[0]["data"];
  assert_eq!(begin["path"]["text"], "b.rs");
  let context = &events[1]["data"];
  assert_eq!(context["lines"]["text"], "aaa\n");
  assert_eq!(context["line_number"], 1);
  assert_eq!(context["absolute_offset"], 0);
  assert_eq!(context["submatches"].as_array().unwrap().len(), 0);

  // matches on the same line coalesce into one event with three submatches
  let matched = &events[2]["data"];
  assert_eq!(matched["lines"]["text"], "bbb bbb bbb\n");
  assert_eq!(matched["line_number"], 2);
  assert_eq!(matched["absolute_offset"], 4);
  assert_eq!(matched["language"], "Rust");
  let submatches = matched["submatches"].as_array().unwrap();
  let offsets: Vec<(u64, u64)> = submatches
    .iter()
    .map(|s| (s["start"].as_u64().unwrap(), s["end"].as_u64().unwrap()))
    .collect();
  assert_eq!(offsets, [(0, 3), (4, 7), (8, 11)]);
  for submatch in submatches {
    assert_eq!(submatch["match"]["text"], "bbb");
  }

  let end = &events[3]["data"];
  assert_eq!(end["path"]["text"], "b.rs");
  assert_eq!(end["binary_offset"], serde_json::Value::Null);
  assert_eq!(end["stats"]["matched_lines"], 1);
  assert_eq!(end["stats"]["matches"], 3);

  let summary = &events[4]["data"];
  assert_eq!(summary["stats"]["searches"], 1);
  assert_eq!(summary["stats"]["searches_with_match"], 1);
  assert_eq!(summary["stats"]["matched_lines"], 1);
  assert_eq!(summary["stats"]["matches"], 3);
  Ok(())
}

/// `--json=ripgrep` emits nothing but a summary when no file matches.
#[test]
fn test_json_ripgrep_no_match() -> Result<()> {
  let dir = create_test_files([("a.js", "alert(123)")])?;
  let output = Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["--json=ripgrep", "-p", "nonexistent-pattern", "."])
    .assert()
    .failure()
    .get_output()
    .stdout
    .clone();
  let events: Vec<Value> = String::from_utf8(output)?
    .lines()
    .map(|l| serde_json::from_str(l).expect("each line is one JSON event"))
    .collect();
  let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
  assert_eq!(types, ["summary"]);
  assert_eq!(events[0]["data"]["stats"]["matches"], 0);
  Ok(())
}

/// The outline command rejects the ripgrep style, it has no match events.
#[test]
fn test_json_ripgrep_outline_rejected() -> Result<()> {
  let dir = create_test_files([("a.js", "export function foo() {}")])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["outline", ".", "--json=ripgrep"])
    .assert()
    .failure()
    .stderr(contains("ripgrep JSON style is not supported for outline"));
  Ok(())
}
