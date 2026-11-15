//! Tests for loading rules from GitHub URLs in `ruleDirs`/`utilDirs`.
//!
//! These tests download a repository tarball from codeload.github.com into a
//! temp cache directory. The pinned commit SHA keeps the remote content
//! stable, and redirecting `XDG_CACHE_HOME` ensures tests never pollute the
//! developer's real cache. codeload is not API-rate-limited, so the tests
//! remain reliable in CI.
//!
//! If GitHub is unreachable, these tests fail with download errors; they do
//! not silently skip, because a broken remote-rule feature must be visible.

mod common;

use anyhow::Result;
use assert_cmd::{Command, cargo_bin};
use common::create_test_files;
use predicates::str::contains;
use tempfile::TempDir;

/// A commit of ast-grep/eslint whose `rules/` and `utils/` directories hold
/// valid ast-grep rules, including rules that reference utility rules.
const RULES_URL: &str = concat!(
  "https://github.com/ast-grep/eslint/tree/",
  "4de4b53b9703816a9ed779edfcf9feeda4982ec0",
  "/rules",
);
const UTILS_URL: &str = concat!(
  "https://github.com/ast-grep/eslint/tree/",
  "4de4b53b9703816a9ed779edfcf9feeda4982ec0",
  "/utils",
);

/// A snippet that triggers the `no-await-in-loop` remote rule, which has
/// warning severity so `sg scan` exits successfully.
const TRIGGER_CODE: &str =
  "async function f() {\n  for (const x of xs) {\n    await g(x);\n  }\n}\n";

/// Run `ast-grep scan` in a temp project configured with remote rules and
/// utils. Returns the project and cache temp dirs so assertions on the cache
/// can be made by the caller.
fn scan_with_remote_rules(filter: &str) -> Result<(TempDir, TempDir)> {
  let cache = TempDir::new()?;
  let config = format!("ruleDirs:\n  - {RULES_URL}\nutilDirs:\n  - {UTILS_URL}\n");
  let dir = create_test_files([
    ("sgconfig.yml", config.as_str()),
    ("src/app.ts", TRIGGER_CODE),
  ])?;
  Command::new(cargo_bin!())
    .env("XDG_CACHE_HOME", cache.path())
    .current_dir(dir.path())
    .args(["scan", "--filter", filter])
    .assert()
    .success()
    .stdout(contains("no-await-in-loop"));
  Ok((dir, cache))
}

#[test]
fn scan_with_github_rule_dirs() -> Result<()> {
  scan_with_remote_rules("no-await-in-loop")?;
  Ok(())
}

#[test]
fn downloaded_rules_are_cached_on_disk() -> Result<()> {
  let (_dir, cache) = scan_with_remote_rules("no-await-in-loop")?;
  // the downloaded rules live under a rule-cache path inside XDG_CACHE_HOME
  let entries = std::fs::read_dir(cache.path())?.count();
  assert!(entries > 0, "rules should be downloaded into the cache");
  Ok(())
}

#[test]
fn remote_and_local_rule_dirs_are_combined() -> Result<()> {
  let cache = TempDir::new()?;
  let config = format!("ruleDirs:\n  - ./rules\n  - {RULES_URL}\nutilDirs:\n  - {UTILS_URL}\n");
  let dir = create_test_files([
    ("sgconfig.yml", config.as_str()),
    (
      "rules/local-rule.yml",
      "id: local-rule\nlanguage: TypeScript\nseverity: warning\nrule:\n  pattern: await g($X)\n",
    ),
    ("src/app.ts", TRIGGER_CODE),
  ])?;
  // both the local and the remote rule fire on the same code
  Command::new(cargo_bin!())
    .env("XDG_CACHE_HOME", cache.path())
    .current_dir(dir.path())
    .args(["scan", "--filter", "local-rule|no-await-in-loop"])
    .assert()
    .success()
    .stdout(contains("local-rule"))
    .stdout(contains("no-await-in-loop"));
  drop(dir);
  drop(cache);
  Ok(())
}

/// The remote `utils/` directory feeds utility rules into remote rules that
/// reference them; without utils the scan would fail to resolve them.
#[test]
fn remote_utils_resolve_rule_references() -> Result<()> {
  let cache = TempDir::new()?;
  let config = format!("ruleDirs:\n  - {RULES_URL}\nutilDirs:\n  - {UTILS_URL}\n");
  let dir = create_test_files([
    ("sgconfig.yml", config.as_str()),
    ("src/app.ts", "if (true) { x = 1; }\n"),
  ])?;
  Command::new(cargo_bin!())
    .env("XDG_CACHE_HOME", cache.path())
    .current_dir(dir.path())
    .args(["scan", "--filter", "no-constant-condition"])
    .assert()
    .success()
    .stdout(contains("no-constant-condition"))
    .stdout(contains("Unexpected constant condition"));
  drop(dir);
  drop(cache);
  Ok(())
}

#[test]
fn missing_github_repo_reports_download_error() -> Result<()> {
  let cache = TempDir::new()?;
  let dir = create_test_files([
    (
      "sgconfig.yml",
      "ruleDirs:\n  - https://github.com/ast-grep/this-repo-does-not-exist/tree/main/rules\n",
    ),
    ("src/app.ts", TRIGGER_CODE),
  ])?;
  Command::new(cargo_bin!())
    .env("XDG_CACHE_HOME", cache.path())
    .current_dir(dir.path())
    .args(["scan"])
    .assert()
    .failure()
    .stderr(contains("Failed to download rules from"));
  drop(dir);
  drop(cache);
  Ok(())
}

/// Regression guard: plain local rule dirs keep working end to end.
#[test]
fn plain_rule_dirs_still_work() -> Result<()> {
  let dir = create_test_files([
    ("sgconfig.yml", "ruleDirs:\n  - ./rules\n"),
    (
      "rules/local.yml",
      "id: local-rule\nlanguage: TypeScript\nseverity: warning\nrule:\n  pattern: debugger\n",
    ),
    ("src/app.ts", "debugger;\n"),
  ])?;
  Command::new(cargo_bin!())
    .current_dir(dir.path())
    .args(["scan", "--filter", "local-rule"])
    .assert()
    .success()
    .stdout(contains("local-rule"));
  drop(dir);
  Ok(())
}
