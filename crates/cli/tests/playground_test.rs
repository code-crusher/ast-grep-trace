mod common;

use assert_cmd::{Command, cargo_bin};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use common::create_test_files;
use predicates::str::contains;

const URL_PREFIX: &str = "https://ast-grep.github.io/playground.html#";

fn sg() -> Command {
  Command::new(cargo_bin!("ast-grep"))
}

fn extract_state_json(stdout: &str) -> serde_json::Value {
  let url = stdout
    .lines()
    .find(|line| line.starts_with(URL_PREFIX))
    .expect("stdout contains the playground URL");
  let fragment = url.split_once('#').unwrap().1;
  let bytes = B64.decode(fragment).expect("valid base64");
  serde_json::from_slice(&bytes).expect("valid json")
}

fn stdout_of(assert: &assert_cmd::assert::Assert) -> String {
  String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

#[test]
fn test_playground_with_file_only() {
  let dir = create_test_files([("hello.ts", "const x: number = 1;\n")]).unwrap();
  let assert = sg()
    .current_dir(dir.path())
    .args(["playground", "--file", "hello.ts", "--print"])
    .assert()
    .success()
    .stdout(contains(URL_PREFIX));

  let state = extract_state_json(&stdout_of(&assert));
  assert_eq!(state["mode"], "Patch");
  assert_eq!(state["lang"], "typescript");
  assert_eq!(state["source"], "const x: number = 1;\n");
  assert_eq!(state["config"], "");
}

#[test]
fn test_playground_with_rule_file() {
  let yaml = "id: no-console\nlanguage: typescript\nrule:\n  pattern: console.log($A)\n";
  let dir = create_test_files([("rule.yml", yaml)]).unwrap();
  let assert = sg()
    .current_dir(dir.path())
    .args(["playground", "--rule-file", "rule.yml", "--print"])
    .assert()
    .success();

  let state = extract_state_json(&stdout_of(&assert));
  assert_eq!(state["mode"], "Config");
  assert_eq!(state["lang"], "typescript");
  assert_eq!(state["source"], "");
  assert_eq!(state["config"], yaml);
}

#[test]
fn test_playground_with_rule_id_in_project() {
  let rule = "id: no-console\nlanguage: typescript\nrule:\n  pattern: console.log($A)\n";
  let dir = create_test_files([
    ("sgconfig.yml", "ruleDirs:\n  - rules\n"),
    ("rules/no-console.yml", rule),
  ])
  .unwrap();
  let assert = sg()
    .current_dir(dir.path())
    .args(["playground", "--rule", "no-console", "--print"])
    .assert()
    .success();

  let state = extract_state_json(&stdout_of(&assert));
  assert_eq!(state["mode"], "Config");
  assert_eq!(state["lang"], "typescript");
  assert!(
    state["config"]
      .as_str()
      .unwrap()
      .contains("console.log($A)")
  );
}

#[test]
fn test_playground_with_file_and_rule() {
  let rule = "id: no-console\nlanguage: typescript\nrule:\n  pattern: console.log($A)\n";
  let dir = create_test_files([
    ("sgconfig.yml", "ruleDirs:\n  - rules\n"),
    ("rules/no-console.yml", rule),
    ("app.ts", "console.log(1)\n"),
  ])
  .unwrap();
  let assert = sg()
    .current_dir(dir.path())
    .args([
      "playground",
      "--file",
      "app.ts",
      "--rule",
      "no-console",
      "--print",
    ])
    .assert()
    .success();

  let state = extract_state_json(&stdout_of(&assert));
  assert_eq!(state["mode"], "Config");
  assert_eq!(state["source"], "console.log(1)\n");
  assert!(
    state["config"]
      .as_str()
      .unwrap()
      .contains("console.log($A)")
  );
}

#[test]
fn test_playground_lang_override() {
  let dir = create_test_files([("hello.tsx", "const x = 1;\n")]).unwrap();
  let assert = sg()
    .current_dir(dir.path())
    .args([
      "playground",
      "--file",
      "hello.tsx",
      "--lang",
      "js",
      "--print",
    ])
    .assert()
    .success();

  let state = extract_state_json(&stdout_of(&assert));
  assert_eq!(state["lang"], "javascript");
}

#[test]
fn test_playground_no_input_errors() {
  sg()
    .args(["playground", "--print"])
    .assert()
    .failure()
    .stderr(contains("nothing to share"));
}

#[test]
fn test_playground_rule_and_rule_file_conflict() {
  let dir = create_test_files([(
    "rule.yml",
    "id: r\nlanguage: javascript\nrule:\n  pattern: x\n",
  )])
  .unwrap();
  sg()
    .current_dir(dir.path())
    .args(["playground", "--rule", "r", "--rule-file", "rule.yml"])
    .assert()
    .failure()
    .stderr(contains("cannot be used with"));
}

#[test]
fn test_playground_unsupported_language_errors() {
  let dir = create_test_files([("main.dart", "void main() {}\n")]).unwrap();
  sg()
    .current_dir(dir.path())
    .args(["playground", "--file", "main.dart", "--print"])
    .assert()
    .failure()
    .stderr(contains("not supported by the web playground"));
}
