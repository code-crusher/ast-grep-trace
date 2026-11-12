mod common;

use anyhow::Result;
use assert_cmd::{Command, cargo_bin};
use common::create_test_files;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

const PLUGIN_CONFIG: &str = r#"
ruleDirs: [rules]
plugins:
  starts-with-foo:
    libraryPath: missing-plugin.so
"#;

const RULE: &str = r#"
id: foo-identifier
language: Tsx
rule:
  all:
    - kind: identifier
    - plugin: starts-with-foo
"#;

const RULE_TEST: &str = r#"
id: foo-identifier
valid:
  - "const bar = 1;"
invalid:
  - "const foo = 1;"
"#;

/// A plugin matching identifiers whose text starts with `foo`.
const PLUGIN_SOURCE: &str = r#"
#[unsafe(no_mangle)]
pub extern "C" fn ast_grep_plugin_match(input: *const u8, input_len: usize) -> i32 {
  let bytes = unsafe { std::slice::from_raw_parts(input, input_len) };
  let json = String::from_utf8_lossy(bytes);
  i32::from(json.contains("\"text\":\"foo"))
}
"#;

fn dylib_name() -> &'static str {
  if cfg!(target_os = "windows") {
    "plugin.dll"
  } else if cfg!(target_os = "macos") {
    "libplugin.dylib"
  } else {
    "libplugin.so"
  }
}

/// Compile a native plugin shared object into `dir`, or `None` if no Rust
/// toolchain is available to build one.
fn build_plugin(dir: &Path) -> Option<PathBuf> {
  let source = dir.join("plugin.rs");
  std::fs::write(&source, PLUGIN_SOURCE).ok()?;
  let output = dir.join(dylib_name());
  let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
  let status = StdCommand::new(rustc)
    .args(["--edition", "2024", "--crate-type", "cdylib", "-o"])
    .arg(&output)
    .arg(&source)
    .status()
    .ok()?;
  status.success().then_some(output)
}

fn project_with_plugin(plugin: &Path) -> Result<tempfile::TempDir> {
  let config = format!(
    "ruleDirs: [rules]\nplugins:\n  starts-with-foo:\n    libraryPath: {}\n",
    plugin.display()
  );
  create_test_files([
    ("sgconfig.yml", config.as_str()),
    ("rules/foo.yml", RULE),
    ("test.tsx", "const foo = 1; const bar = 2;"),
  ])
}

#[test]
fn plugins_are_ignored_by_default() -> Result<()> {
  let dir = create_test_files([
    ("sgconfig.yml", PLUGIN_CONFIG),
    ("rules/foo.yml", RULE),
    ("test.tsx", "const foo = 1;"),
  ])?;

  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .arg("scan")
    .assert()
    .failure()
    .stderr(contains("plugins are ignored"))
    .stderr(contains("Plugin `starts-with-foo` is not registered"));
  Ok(())
}

#[test]
fn explicit_ignore_skips_plugins() -> Result<()> {
  let dir = create_test_files([
    ("sgconfig.yml", PLUGIN_CONFIG),
    ("rules/foo.yml", RULE),
    ("test.tsx", "const foo = 1;"),
  ])?;

  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["scan", "--custom-languages", "ignore"])
    .assert()
    .failure()
    .stderr(contains("plugins are ignored"))
    .stderr(contains("Plugin `starts-with-foo` is not registered"));
  Ok(())
}

#[test]
fn allow_plugins_attempts_to_load_the_library() -> Result<()> {
  let dir = create_test_files([
    ("sgconfig.yml", PLUGIN_CONFIG),
    ("rules/foo.yml", RULE),
    ("test.tsx", "const foo = 1;"),
  ])?;

  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["scan", "--custom-languages", "allow"])
    .assert()
    .failure()
    .stderr(contains("Cannot load plugin library"));
  Ok(())
}

#[test]
fn allowed_plugin_matches_with_custom_rule() -> Result<()> {
  let scratch = tempfile::tempdir()?;
  let Some(plugin) = build_plugin(scratch.path()) else {
    return Ok(());
  };
  let dir = project_with_plugin(&plugin)?;

  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["scan", "--custom-languages", "allow", "--json"])
    .assert()
    .success()
    .stdout(contains("\"text\": \"foo\""))
    .stdout(contains("\"text\": \"bar\"").not());
  Ok(())
}

#[test]
fn allowed_plugin_runs_in_test_subcommand() -> Result<()> {
  let scratch = tempfile::tempdir()?;
  let Some(plugin) = build_plugin(scratch.path()) else {
    return Ok(());
  };
  let config = format!(
    "ruleDirs: [rules]\ntestConfigs:\n  - testDir: rule-tests\nplugins:\n  starts-with-foo:\n    libraryPath: {}\n",
    plugin.display()
  );
  let dir = create_test_files([
    ("sgconfig.yml", config.as_str()),
    ("rules/foo.yml", RULE),
    ("rule-tests/foo-test.yml", RULE_TEST),
  ])?;

  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args([
      "test",
      "--custom-languages",
      "allow",
      "--skip-snapshot-tests",
    ])
    .assert()
    .success();
  Ok(())
}
