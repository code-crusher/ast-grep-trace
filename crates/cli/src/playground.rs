use std::fs::read_to_string;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use anyhow::{Result, anyhow};
use ast_grep_config::{GlobalRules, RuleConfig, from_multiple, from_yaml_string};
use ast_grep_core::language::Language;
use ast_grep_language::SupportLang;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::config::ProjectConfig;
use crate::lang::SgLang;
use crate::utils::RuleOverwrite;

const PLAYGROUND_BASE: &str = "https://ast-grep.github.io/playground.html";

/// State shared with the web playground via the URL fragment.
///
/// Mirrors the `State` shape declared in the playground frontend. The
/// frontend merges this over its default state, so empty fields are filled in.
#[derive(Serialize, Default, Debug)]
pub struct PlaygroundState {
  pub mode: String,
  pub query: String,
  pub rewrite: String,
  pub config: String,
  pub source: String,
  pub strictness: String,
  pub selector: String,
  pub lang: String,
}

/// Encode the state as `base64(utf8(json(state)))` and append it to the
/// playground URL as a fragment, matching the frontend's own encoding.
pub fn build_url(state: &PlaygroundState) -> String {
  let json = serde_json::to_string(state).expect("PlaygroundState is always serializable");
  let fragment = B64.encode(json.as_bytes());
  format!("{PLAYGROUND_BASE}#{fragment}")
}

/// Pick the playground `lang` field, preferring `--lang` > the rule's
/// `language` > the file's extension.
pub(crate) fn resolve_lang(
  flag: Option<&str>,
  rule_lang: Option<SgLang>,
  file: Option<&Path>,
) -> Result<String> {
  if let Some(lang) = flag {
    let lang = SgLang::from_str(lang).map_err(|_| anyhow!("unsupported --lang value: {lang}"))?;
    return playground_lang_name(lang);
  }
  if let Some(lang) = rule_lang {
    return playground_lang_name(lang);
  }
  if let Some(lang) = file.and_then(SgLang::from_path) {
    return playground_lang_name(lang);
  }
  Err(anyhow!("could not infer language; pass --lang explicitly"))
}

/// Map an `SgLang` to the identifier the playground expects, or error if the
/// language has no parser in the web playground.
fn playground_lang_name(lang: SgLang) -> Result<String> {
  let name = match lang {
    SgLang::Builtin(builtin) => match builtin {
      SupportLang::Bash => "bash",
      SupportLang::C => "c",
      SupportLang::Cpp => "cpp",
      SupportLang::CSharp => "csharp",
      SupportLang::Css => "css",
      SupportLang::Elixir => "elixir",
      SupportLang::Go => "go",
      SupportLang::Hcl => "hcl",
      SupportLang::Html => "html",
      SupportLang::Java => "java",
      SupportLang::JavaScript => "javascript",
      SupportLang::Json => "json",
      SupportLang::Kotlin => "kotlin",
      SupportLang::Lua => "lua",
      SupportLang::Nix => "nix",
      SupportLang::Php => "php",
      SupportLang::Python => "python",
      SupportLang::Ruby => "ruby",
      SupportLang::Rust => "rust",
      SupportLang::Scala => "scala",
      SupportLang::Swift => "swift",
      SupportLang::Tsx => "tsx",
      SupportLang::TypeScript => "typescript",
      SupportLang::Yaml => "yaml",
      SupportLang::Dart
      | SupportLang::Haskell
      | SupportLang::Markdown
      | SupportLang::Solidity
      | SupportLang::Zig => return Err(unsupported_playground_lang(lang)),
    },
    SgLang::Custom(_) => return Err(unsupported_playground_lang(lang)),
  };
  Ok(name.into())
}

fn unsupported_playground_lang(lang: SgLang) -> anyhow::Error {
  let name = lang.to_string().to_lowercase();
  anyhow!("language '{name}' is not supported by the web playground")
}

/// Read the source file, or return an empty string when no file is given.
pub(crate) fn resolve_source(file: Option<&Path>) -> Result<String> {
  match file {
    None => Ok(String::new()),
    Some(path) => read_to_string(path)
      .map_err(|e| anyhow!("failed to read source file {}: {e}", path.display())),
  }
}

/// Result of rule resolution: (YAML text, optional language).
type ResolvedRule = (String, Option<SgLang>);

/// Resolve `--rule-file` or `--rule <ID>` into YAML text for the playground,
/// along with the rule's language. Returns `None` when only `--file` is given.
pub(crate) fn resolve_rule(
  rule_id: Option<&str>,
  rule_file: Option<&Path>,
  project: Option<&ProjectConfig>,
) -> Result<Option<ResolvedRule>> {
  if let Some(path) = rule_file {
    let yaml = read_to_string(path)
      .map_err(|e| anyhow!("failed to read rule file {}: {e}", path.display()))?;
    let lang = infer_rule_file_lang(&yaml)?;
    return Ok(Some((yaml, lang)));
  }
  if let Some(id) = rule_id {
    let project = project.ok_or_else(|| {
      anyhow!("--rule '{id}' requires a project config (sgconfig.yml) - not found")
    })?;
    let (collection, _) = project
      .find_rules(RuleOverwrite::default())
      .map_err(|e| anyhow!("failed to load project rules: {e}"))?;
    let rule = collection
      .get_rule(id)
      .ok_or_else(|| anyhow!("rule id '{id}' not found in project config"))?;
    let yaml = rule_to_yaml(rule)?;
    ensure_self_contained(&yaml, &rule.id)?;
    return Ok(Some((yaml, Some(rule.language))));
  }
  Ok(None)
}

/// A single document of a rule file. Only the language is needed here, so
/// rules referencing project utilities are not fully deserialized.
#[derive(Deserialize)]
struct RuleFileDoc {
  language: Option<SgLang>,
}

/// Extract the `language:` field from the rule file without validating the
/// rules themselves. For multi-document YAML the last document's language wins.
fn infer_rule_file_lang(yaml: &str) -> Result<Option<SgLang>> {
  let docs: Vec<RuleFileDoc> =
    from_multiple(yaml).map_err(|e| anyhow!("failed to parse rule file YAML: {e}"))?;
  Ok(docs.into_iter().rev().find_map(|doc| doc.language))
}

/// Serialize a `RuleConfig` back to YAML for sharing with the playground.
///
/// The config type has many optional fields, so serialization would otherwise
/// emit a `null` line for every one of them. Drop those to keep the shared
/// YAML readable in the playground editor.
fn rule_to_yaml(rule: &RuleConfig<SgLang>) -> Result<String> {
  let value = serde_value::to_value(&**rule)
    .map_err(|e| anyhow!("failed to serialize rule '{}' to YAML: {e}", rule.id))?;
  serde_saphyr::to_string(&strip_serialized_nones(value))
    .map_err(|e| anyhow!("failed to serialize rule '{}' to YAML: {e}", rule.id))
}

/// Recursively remove `Option::None` entries from a serialized value.
/// `metadata` is user-controlled free-form data and is left untouched.
fn strip_serialized_nones(value: serde_value::Value) -> serde_value::Value {
  use serde_value::Value;
  match value {
    Value::Map(map) => Value::Map(
      map
        .into_iter()
        .filter(|(_, value)| !matches!(value, Value::Option(None)))
        .map(|(key, value)| {
          let is_metadata = matches!(&key, Value::String(key) if key == "metadata");
          let value = if is_metadata {
            value
          } else {
            strip_serialized_nones(value)
          };
          (key, value)
        })
        .collect(),
    ),
    Value::Seq(seq) => Value::Seq(seq.into_iter().map(strip_serialized_nones).collect()),
    Value::Newtype(value) => Value::Newtype(Box::new(strip_serialized_nones(*value))),
    Value::Option(Some(value)) => Value::Option(Some(Box::new(strip_serialized_nones(*value)))),
    Value::Bool(_)
    | Value::U8(_)
    | Value::U16(_)
    | Value::U32(_)
    | Value::U64(_)
    | Value::I8(_)
    | Value::I16(_)
    | Value::I32(_)
    | Value::I64(_)
    | Value::F32(_)
    | Value::F64(_)
    | Value::Char(_)
    | Value::String(_)
    | Value::Unit
    | Value::Option(None)
    | Value::Bytes(_) => value,
  }
}

/// Project utility rules are not serialized into playground URLs, so a shared
/// rule must parse standalone. This also catches other invalid serializations.
fn ensure_self_contained(yaml: &str, rule_id: &str) -> Result<()> {
  from_yaml_string::<SgLang>(yaml, &GlobalRules::default()).map_err(|e| {
    anyhow!(
      "rule '{rule_id}' references project utilities or is otherwise not self-contained and cannot be shared with the playground: {e}"
    )
  })?;
  Ok(())
}

#[derive(Args)]
pub struct PlaygroundArg {
  /// Source file to load into the playground.
  #[clap(short, long, value_name = "FILE")]
  pub file: Option<PathBuf>,

  /// Rule ID to look up in the project config and load into the playground.
  #[clap(short, long, value_name = "ID", conflicts_with = "rule_file")]
  pub rule: Option<String>,

  /// Read rule YAML from a path (alternative to --rule).
  #[clap(long, value_name = "PATH")]
  pub rule_file: Option<PathBuf>,

  /// Language override. Inferred from --file extension or the rule's language.
  #[clap(short, long, value_name = "LANG")]
  pub lang: Option<String>,

  /// Print the URL only; do not open the browser.
  #[clap(long)]
  pub print: bool,
}

/// Entry point for `ast-grep playground`: resolve the requested rule and/or
/// source, build the playground URL, then open it in the browser or print it.
pub fn run_playground(arg: PlaygroundArg, project: Result<ProjectConfig>) -> Result<ExitCode> {
  if arg.file.is_none() && arg.rule.is_none() && arg.rule_file.is_none() {
    return Err(anyhow!(
      "nothing to share - pass --file and/or --rule (or --rule-file)"
    ));
  }

  let project = project.as_ref().ok();
  let (config, rule_lang) =
    match resolve_rule(arg.rule.as_deref(), arg.rule_file.as_deref(), project)? {
      Some((yaml, lang)) => (yaml, lang),
      None => (String::new(), None),
    };
  let source = resolve_source(arg.file.as_deref())?;
  let lang = resolve_lang(arg.lang.as_deref(), rule_lang, arg.file.as_deref())?;
  let mode = if config.is_empty() { "Patch" } else { "Config" };
  let state = PlaygroundState {
    mode: mode.into(),
    lang,
    source,
    config,
    ..PlaygroundState::default()
  };
  let url = build_url(&state);

  if arg.print {
    println!("{url}");
    return Ok(ExitCode::SUCCESS);
  }

  eprintln!("Opening playground in your browser...");
  if let Err(e) = open::that(&url) {
    eprintln!("Could not open the browser automatically: {e}");
    eprintln!("Open this URL manually:");
    println!("{url}");
  }
  Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::NamedTempFile;

  fn decode_state(url: &str) -> serde_json::Value {
    let fragment = url.split_once('#').expect("url has fragment").1;
    let bytes = B64.decode(fragment).expect("valid base64");
    serde_json::from_slice(&bytes).expect("valid json")
  }

  fn test_project(dir: &Path, util_dirs: Option<Vec<PathBuf>>) -> ProjectConfig {
    ProjectConfig {
      project_dir: dir.to_path_buf(),
      rule_dirs: vec![PathBuf::from("rules")],
      outline_rules: vec![],
      test_configs: None,
      util_dirs,
      no_ignore: None,
    }
  }

  #[test]
  fn build_url_encodes_state_round_trip() {
    let state = PlaygroundState {
      mode: "Config".into(),
      lang: "typescript".into(),
      source: "console.log(1)".into(),
      config: "rule:\n  pattern: console.log($A)\n".into(),
      ..PlaygroundState::default()
    };
    let url = build_url(&state);
    assert!(url.starts_with("https://ast-grep.github.io/playground.html#"));
    let decoded = decode_state(&url);
    assert_eq!(decoded["mode"], "Config");
    assert_eq!(decoded["lang"], "typescript");
    assert_eq!(decoded["source"], "console.log(1)");
    assert_eq!(decoded["config"], "rule:\n  pattern: console.log($A)\n");
    assert_eq!(decoded["query"], "");
    assert_eq!(decoded["rewrite"], "");
    assert_eq!(decoded["strictness"], "");
    assert_eq!(decoded["selector"], "");
  }

  #[test]
  fn build_url_handles_unicode_source() {
    let state = PlaygroundState {
      mode: "Patch".into(),
      lang: "javascript".into(),
      source: "// 你好 🦀".into(),
      ..PlaygroundState::default()
    };
    let decoded = decode_state(&build_url(&state));
    assert_eq!(decoded["source"], "// 你好 🦀");
  }

  #[test]
  fn resolve_lang_prefers_explicit_flag() {
    let got = resolve_lang(
      Some("typescript"),
      Some(SgLang::from_str("rust").unwrap()),
      Some(Path::new("foo.js")),
    )
    .unwrap();
    assert_eq!(got, "typescript");
  }

  #[test]
  fn resolve_lang_falls_back_to_rule_language() {
    let got = resolve_lang(
      None,
      Some(SgLang::from_str("rust").unwrap()),
      Some(Path::new("foo.js")),
    )
    .unwrap();
    assert_eq!(got, "rust");
  }

  #[test]
  fn resolve_lang_falls_back_to_file_extension() {
    let got = resolve_lang(None, None, Some(Path::new("foo.tsx"))).unwrap();
    assert_eq!(got, "tsx");
  }

  #[test]
  fn resolve_lang_normalizes_aliases() {
    let got = resolve_lang(Some("JS"), None, None).unwrap();
    assert_eq!(got, "javascript");
  }

  #[test]
  fn resolve_lang_errors_when_nothing_known() {
    let err = resolve_lang(None, None, None).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("--lang"));
  }

  #[test]
  fn resolve_lang_rejects_playground_unsupported_lang() {
    for (flag, file) in [(Some("dart"), None), (None, Some(Path::new("main.dart")))] {
      let err = resolve_lang(flag, None, file).unwrap_err();
      let msg = err.to_string();
      assert!(msg.contains("dart"), "{msg}");
      assert!(msg.contains("playground"), "{msg}");
    }
  }

  #[test]
  fn resolve_source_reads_file_contents() {
    let tmp = NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "let x = 1;").unwrap();
    assert_eq!(resolve_source(Some(tmp.path())).unwrap(), "let x = 1;");
  }

  #[test]
  fn resolve_source_returns_empty_without_file() {
    assert_eq!(resolve_source(None).unwrap(), "");
  }

  #[test]
  fn resolve_source_errors_on_missing_file() {
    let err = resolve_source(Some(Path::new("/definitely/nonexistent/path"))).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("read"));
  }

  #[test]
  fn resolve_rule_returns_none_without_input() {
    assert!(resolve_rule(None, None, None).unwrap().is_none());
  }

  #[test]
  fn resolve_rule_reads_rule_file_verbatim() {
    let tmp = NamedTempFile::new().unwrap();
    let yaml = "id: test\nlanguage: javascript\nrule:\n  pattern: foo($A)\n";
    std::fs::write(tmp.path(), yaml).unwrap();
    let (text, lang) = resolve_rule(None, Some(tmp.path()), None).unwrap().unwrap();
    assert_eq!(text, yaml);
    assert_eq!(lang.unwrap(), SgLang::from_str("javascript").unwrap());
  }

  #[test]
  fn resolve_rule_reads_rule_file_language_without_project_utils() {
    let tmp = NamedTempFile::new().unwrap();
    let yaml = "id: test\nlanguage: javascript\nrule:\n  matches: project-util\n";
    std::fs::write(tmp.path(), yaml).unwrap();
    let (_, lang) = resolve_rule(None, Some(tmp.path()), None).unwrap().unwrap();
    assert_eq!(lang.unwrap(), SgLang::from_str("javascript").unwrap());
  }

  #[test]
  fn resolve_rule_serializes_project_rule() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("rules")).unwrap();
    std::fs::write(
      temp.path().join("rules/no-console.yml"),
      "id: no-console\nlanguage: typescript\nrule:\n  pattern: console.log($A)\n",
    )
    .unwrap();
    let project = test_project(temp.path(), None);

    let (yaml, lang) = resolve_rule(Some("no-console"), None, Some(&project))
      .unwrap()
      .unwrap();
    assert_eq!(lang.unwrap(), SgLang::from_str("typescript").unwrap());
    assert!(yaml.contains("id: no-console"), "{yaml}");
    assert!(yaml.contains("pattern: console.log($A)"), "{yaml}");
    // serialized rule must parse standalone in the playground
    from_yaml_string::<SgLang>(&yaml, &GlobalRules::default()).expect("shared YAML parses alone");
  }

  #[test]
  fn resolve_rule_errors_on_project_util_reference() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("rules")).unwrap();
    std::fs::create_dir(temp.path().join("utils")).unwrap();
    std::fs::write(
      temp.path().join("utils/num.yml"),
      "id: num\nlanguage: typescript\nrule:\n  kind: number\n",
    )
    .unwrap();
    std::fs::write(
      temp.path().join("rules/use-num.yml"),
      "id: use-num\nlanguage: typescript\nrule:\n  matches: num\n",
    )
    .unwrap();
    let project = test_project(temp.path(), Some(vec![PathBuf::from("utils")]));

    let err = resolve_rule(Some("use-num"), None, Some(&project)).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("not self-contained"), "{msg}");
  }

  #[test]
  fn resolve_rule_errors_when_id_missing_and_no_project() {
    let err = resolve_rule(Some("does-not-exist"), None, None).unwrap_err();
    assert!(err.to_string().contains("does-not-exist"));
  }

  #[test]
  fn run_playground_errors_when_no_input() {
    let arg = PlaygroundArg {
      file: None,
      rule: None,
      rule_file: None,
      lang: None,
      print: true,
    };
    let err = run_playground(arg, Err(anyhow!("no project"))).unwrap_err();
    assert!(err.to_string().contains("nothing to share"));
  }
}
