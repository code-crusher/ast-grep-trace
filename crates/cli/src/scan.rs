use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use ast_grep_config::{
  CombinedScan, GlobalRules, NO_SUPPRESS_ALL_ID, RuleCollection, RuleConfig, Severity,
  UNUSED_SUPPRESSION_ID, from_yaml_string,
};
use ast_grep_core::{NodeMatch, tree_sitter::StrDoc};
use ast_grep_language::SupportLang;
use clap::Args;
use ignore::WalkParallel;

use crate::config::{ProjectConfig, read_rule_file, with_rule_stats};
use crate::lang::SgLang;
use crate::print::{
  CloudPrinter, ColoredPrinter, Diff, FileNamePrinter, InteractivePrinter, JSONPrinter, Platform,
  PrintProcessor, Printer, ReportStyle, SimpleFile,
};
use crate::utils::RuleOverwrite;
use crate::utils::debugger::{DebugRule, run_debug_session};
use crate::utils::{ContextArgs, InputArgs, OutputArgs, OverwriteArgs, filter_file_rule};
use crate::utils::{ErrorContext as EC, MaxItemCounter};
use crate::utils::{FileTrace, ScanTrace};
use crate::utils::{Items, PathWorker, StdInWorker, Worker};

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Args)]
pub struct ScanArg {
  /// Scan the codebase with the single rule located at the path RULE_FILE.
  ///
  /// It is useful to run single rule without project setup or sgconfig.yml.
  #[clap(short, long, value_name = "RULE_FILE")]
  rule: Option<PathBuf>,

  /// Scan the codebase with a rule defined by the provided RULE_TEXT.
  ///
  /// Use this argument if you want to test a rule without creating a YAML file on disk.
  /// You can run multiple rules by separating them with `---` in the RULE_TEXT.
  /// --inline-rules is incompatible with --rule.
  #[clap(long, conflicts_with = "rule", value_name = "RULE_TEXT")]
  inline_rules: Option<String>,

  /// Output warning/error messages in different formats.
  ///
  /// Supported formats: GitHub Action, SARIF (Static Analysis Results Interchange Format).
  #[clap(long, conflicts_with = "json", conflicts_with = "interactive")]
  format: Option<Platform>,

  #[clap(long, default_value = "rich", conflicts_with = "json")]
  report_style: ReportStyle,

  /// Include rule metadata in the json output.
  ///
  /// This flags requires --json output. Default is false.
  #[clap(long, requires = "json")]
  include_metadata: bool,

  /// severity related options
  #[clap(flatten)]
  overwrite: OverwriteArgs,

  /// input related options
  #[clap(flatten)]
  input: InputArgs,
  /// output related options
  #[clap(flatten)]
  output: OutputArgs,
  /// context related options
  #[clap(flatten)]
  context: ContextArgs,

  /// Show at most NUM results and stop running once the limit is reached.
  ///
  /// Useful for big codebase to fail scan/search fast.
  #[clap(long, conflicts_with = "interactive", value_name = "NUM")]
  max_results: Option<u16>,

  /// Pause on every candidate node and start an interactive debugger.
  ///
  /// The debugger lets you inspect the current node, the rule under test and
  /// the metavariables captured so far, which helps to find out why a rule
  /// does or does not match. It reads commands from stdin, so it cannot be
  /// used together with --stdin or the output formats that own stdout.
  #[clap(
    long,
    conflicts_with_all = ["stdin", "json", "format", "files_with_matches", "interactive", "update_all"]
  )]
  debug: bool,
}

impl ScanArg {
  // whether the scan includes all rules available in the project
  fn include_all_rules(&self) -> bool {
    self.overwrite.include_all_rules() && self.rule.is_none() && self.inline_rules.is_none()
  }
}

pub fn run_with_config(mut arg: ScanArg, project: Result<ProjectConfig>) -> Result<ExitCode> {
  if let Ok(config) = &project {
    arg.input.apply_config(config);
  }
  let project_trace = arg.output.inspect.project_trace();
  project_trace.print_project(&project)?;
  let context = arg.context.get();
  if arg.debug {
    // the debugger owns the terminal, so it takes precedence over other printers
    let printer = ColoredPrinter::stdout(arg.output.color)
      .style(arg.report_style)
      .context(context);
    return run_scan_with_debugger(arg, printer, project);
  }
  if arg.output.files_with_matches {
    let printer = FileNamePrinter::stdout(arg.output.color);
    return run_scan(arg, printer, project);
  }
  if let Some(format) = &arg.format {
    let printer = CloudPrinter::stdout(format.clone());
    return run_scan(arg, printer, project);
  }
  if let Some(json) = arg.output.json {
    let printer = JSONPrinter::stdout(json).include_metadata(arg.include_metadata);
    return run_scan(arg, printer, project);
  }
  let printer = ColoredPrinter::stdout(arg.output.color)
    .style(arg.report_style)
    .context(context);
  let interactive = arg.output.needs_interactive();
  if interactive {
    let from_stdin = arg.input.stdin;
    let printer = InteractivePrinter::new(printer, arg.output.update_all, from_stdin)?;
    run_scan(arg, printer, project)
  } else {
    run_scan(arg, printer, project)
  }
}

/// Sequential counterpart of `run_scan` used by the debugger.
///
/// The debugger needs to read commands from stdin and pause between files, so
/// the parallel worker pipeline cannot be used.
fn run_scan_with_debugger<P: Printer + 'static>(
  arg: ScanArg,
  mut printer: P,
  project: Result<ProjectConfig>,
) -> Result<ExitCode> {
  let worker = ScanWithConfig::try_new(arg, project)?;
  let colored = worker.arg.output.color.should_use_color();
  let processor = printer.get_processor();
  let mut error_count = 0usize;
  printer.before_print()?;
  let mut quit = false;
  let mut paused = false;
  for entry in worker.build_walk_seq()? {
    let entry = match entry {
      Ok(entry) => entry,
      Err(err) => {
        eprintln!("ERROR: {err}");
        continue;
      }
    };
    if !entry.file_type().is_some_and(|ty| ty.is_file()) {
      continue;
    }
    let path = entry.path();
    let units = match filter_file_rule(path, &worker.configs, &worker.trace) {
      Ok(units) => units,
      Err(err) => {
        eprintln!("ERROR: {err}");
        continue;
      }
    };
    for grep in units {
      let file_content = grep.source();
      let abs_path = path.canonicalize()?;
      let normalized_path = abs_path.strip_prefix(&worker.proj_dir).unwrap_or(path);
      let rules = worker
        .configs
        .get_rule_from_lang(normalized_path, *grep.lang());
      // RuleCollection already excludes rules with `severity: off`
      let debug_rules: Vec<_> = rules
        .iter()
        .map(|rule| DebugRule {
          label: rule.id.clone(),
          description: debug_rule_description(rule),
          matcher: &rule.matcher,
        })
        .collect();
      if debug_rules.is_empty() {
        continue;
      }
      let outcome = run_debug_session(&grep, path, &debug_rules, colored)?;
      quit = outcome.quit;
      paused = paused || outcome.candidates > 0;
      if quit {
        break;
      }
      let combined = CombinedScan::new(rules);
      // exclude_fix rule because diffs are printed separately
      let scanned = combined.scan(&grep, /* separate_fix */ false);
      for (rule, matches) in scanned.matches {
        if matches.is_empty() {
          continue;
        }
        if matches!(rule.severity, Severity::Error) {
          error_count = error_count.saturating_add(matches.len());
        }
        let processed = match_rule_on_file(path, matches, rule, file_content, &processor)?;
        printer.process(processed)?;
      }
    }
    if quit {
      break;
    }
  }
  printer.after_print()?;
  worker.trace.print()?;
  if !paused {
    eprintln!("no candidate node found in the given paths");
  }
  // quitting the debugger is a deliberate user action, not a failed scan
  if quit {
    return Ok(ExitCode::SUCCESS);
  }
  if error_count > 0 {
    Err(anyhow::anyhow!(EC::DiagnosticError(error_count)))
  } else {
    Ok(ExitCode::SUCCESS)
  }
}

/// Render a rule as YAML for the debugger's `print-rule` command.
fn debug_rule_description(rule: &RuleConfig<SgLang>) -> String {
  // RuleConfig derefs to its serializable form
  let serializable: &ast_grep_config::SerializableRuleConfig<SgLang> = rule;
  serde_saphyr::to_string(serializable)
    .unwrap_or_else(|err| format!("<cannot serialize rule: {err}>"))
}

/// Ad-hoc rules can reference global utils declared in `sgconfig.yml`'s utilDirs.
/// If no project config is found, fall back to no global rules.
fn global_rules_from_project(project: &Result<ProjectConfig>) -> Result<GlobalRules> {
  match project {
    Ok(config) => config.find_utils(),
    Err(_) => Ok(GlobalRules::default()),
  }
}

fn run_scan<P: Printer + 'static>(
  arg: ScanArg,
  printer: P,
  project: Result<ProjectConfig>,
) -> Result<ExitCode> {
  if arg.input.stdin {
    let worker = ScanStdin::try_new(arg, project)?;
    // TODO: report a soft error if rules have different languages
    worker.run_std_in(printer)
  } else {
    let worker = ScanWithConfig::try_new(arg, project)?;
    worker.run_path(printer)
  }
}

struct ScanWithConfig {
  arg: ScanArg,
  configs: RuleCollection<SgLang>,
  unused_suppression_rule: RuleConfig<SgLang>,
  no_suppress_all_rule: RuleConfig<SgLang>,
  trace: ScanTrace,
  proj_dir: PathBuf,
  // TODO: remove this
  error_count: AtomicUsize,
  max_item_counter: Option<MaxItemCounter>,
}
impl ScanWithConfig {
  fn try_new(arg: ScanArg, project: Result<ProjectConfig>) -> Result<Self> {
    let overwrite = RuleOverwrite::new(&arg.overwrite)?;
    let unused_suppression_rule = unused_suppression_rule_config(&arg, &overwrite);
    let no_suppress_all_rule = no_suppress_all_rule_config(&overwrite);
    let mut proj_dir = PathBuf::from(".");
    let (configs, rule_trace) = if let Some(path) = &arg.rule {
      let global_rules = global_rules_from_project(&project)?;
      let rules = read_rule_file(path, &global_rules)
        .and_then(|configs| overwrite.process_configs(configs))?;
      proj_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
      with_rule_stats(rules)?
    } else if let Some(text) = &arg.inline_rules {
      let global_rules = global_rules_from_project(&project)?;
      let configs = from_yaml_string(text, &global_rules)
        .with_context(|| EC::ParseRule("INLINE_RULES".into()))?;
      let rules = overwrite.process_configs(configs)?;
      with_rule_stats(rules)?
    } else {
      // Configured scans require a project; ad-hoc rules can fall back without one.
      let project_config = project?;
      proj_dir = project_config.project_dir.clone();
      project_config.find_rules(overwrite)?
    };
    let trace = arg.output.inspect.scan_trace(rule_trace);
    trace.print_rules(&configs)?;
    let absolute_proj_dir = proj_dir
      .canonicalize()
      .or_else(|_| std::env::current_dir())?;
    let max_item_counter = arg.max_results.map(MaxItemCounter::new);
    Ok(Self {
      arg,
      configs,
      unused_suppression_rule,
      no_suppress_all_rule,
      trace,
      proj_dir: absolute_proj_dir,
      error_count: AtomicUsize::new(0),
      max_item_counter,
    })
  }
}
impl ScanWithConfig {
  fn scan_langs(&self) -> HashSet<SgLang> {
    let mut langs = HashSet::new();
    self.configs.for_each_rule(|rule| {
      langs.insert(rule.language);
    });
    langs
  }

  /// Sequential counterpart of `PathWorker::build_walk` used by the debugger.
  fn build_walk_seq(&self) -> Result<ignore::Walk> {
    self.arg.input.walk_seq_langs(self.scan_langs().into_iter())
  }
}

impl Worker for ScanWithConfig {
  fn consume_items<P: Printer>(
    &self,
    items: Items<P::Processed>,
    mut printer: P,
  ) -> Result<ExitCode> {
    printer.before_print()?;
    for item in items {
      printer.process(item)?;
    }
    printer.after_print()?;
    self.trace.print()?;
    let error_count = self.error_count.load(Ordering::Acquire);
    if error_count > 0 {
      Err(anyhow::anyhow!(EC::DiagnosticError(error_count)))
    } else {
      Ok(ExitCode::SUCCESS)
    }
  }
}

// we should only suggest unused suppression if scan includes all rules
// otherwise, keep silent about unused suppressions because they may used by other rules
// this is a "smart" heuristic but user always can override it
fn default_unused_suppression_rule_severity(arg: &ScanArg) -> Severity {
  if arg.include_all_rules() {
    Severity::Hint
  } else {
    Severity::Off
  }
}

fn no_suppress_all_rule_config(overwrite: &RuleOverwrite) -> RuleConfig<SgLang> {
  let severity = overwrite
    .find(NO_SUPPRESS_ALL_ID)
    .severity
    .unwrap_or(Severity::Off);
  let severity = overwrite.apply_min_severity(severity);
  CombinedScan::no_suppress_all_config(severity, SupportLang::Rust.into())
}

fn unused_suppression_rule_config(arg: &ScanArg, overwrite: &RuleOverwrite) -> RuleConfig<SgLang> {
  let severity = overwrite
    .find(UNUSED_SUPPRESSION_ID)
    .severity
    .unwrap_or_else(|| default_unused_suppression_rule_severity(arg));
  let severity = overwrite.apply_min_severity(severity);
  CombinedScan::unused_config(severity, SupportLang::Rust.into())
}

impl PathWorker for ScanWithConfig {
  fn get_trace(&self) -> &FileTrace {
    &self.trace.inner.file_trace
  }
  fn print_skip(&self, path: &Path, err: &anyhow::Error) -> Result<()> {
    self.trace.print_skip(path, err)
  }
  fn build_walk(&self) -> Result<WalkParallel> {
    self.arg.input.walk_langs(self.scan_langs().into_iter())
  }
  fn produce_item<P: Printer>(
    &self,
    path: &Path,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>> {
    let items = filter_file_rule(path, &self.configs, &self.trace)?;
    let mut error_count = 0usize;
    let mut ret = vec![];
    for grep in items {
      let file_content = grep.source();
      // use path relative to project director
      let abs_path = path.canonicalize()?;
      let normalized_path = abs_path.strip_prefix(&self.proj_dir).unwrap_or(path);
      let rules = self
        .configs
        .get_rule_from_lang(normalized_path, *grep.lang());
      let mut combined = CombinedScan::new(rules);
      combined.set_unused_suppression_rule(&self.unused_suppression_rule);
      combined.set_no_suppress_all_rule(&self.no_suppress_all_rule);
      let interactive = self.arg.output.needs_interactive();
      // exclude_fix rule because we already have diff inspection before
      let scanned = combined.scan(&grep, /* separate_fix*/ interactive);
      if interactive {
        let diffs = scanned.diffs;
        let processed = match_rule_diff_on_file(path, diffs, processor)?;
        ret.push(processed);
      }
      for (rule, matches) in scanned.matches {
        // Atomically reserve slots for matches, truncating if needed
        let matches: Vec<_> = if let Some(counter) = &self.max_item_counter {
          let wanted = matches.len();
          // Atomically claim as many slots as we can (up to wanted)
          let claimed = counter.claim(wanted);
          if claimed == 0 {
            break;
          }
          matches.into_iter().take(claimed).collect()
        } else {
          matches
        };
        if matches.is_empty() {
          continue;
        }
        let match_count = matches.len();
        if matches!(rule.severity, Severity::Error) {
          error_count = error_count.saturating_add(match_count);
        }
        let processed = match_rule_on_file(path, matches, rule, file_content, processor)?;
        ret.push(processed);
      }
    }
    self.error_count.fetch_add(error_count, Ordering::AcqRel);
    Ok(ret)
  }

  fn should_stop(&self) -> bool {
    match &self.max_item_counter {
      Some(max) => max.reached_max(),
      None => false,
    }
  }
}

struct ScanStdin {
  rules: Vec<RuleConfig<SgLang>>,
  // TODO: remove this
  error_count: AtomicUsize,
  max_diagnostics_shown: Option<usize>,
}
impl ScanStdin {
  fn try_new(arg: ScanArg, project: Result<ProjectConfig>) -> Result<Self> {
    let overwrite = RuleOverwrite::new(&arg.overwrite)?;
    let rules = if let Some(path) = &arg.rule {
      let global_rules = global_rules_from_project(&project)?;
      read_rule_file(path, &global_rules).and_then(|configs| overwrite.process_configs(configs))?
    } else if let Some(text) = &arg.inline_rules {
      let global_rules = global_rules_from_project(&project)?;
      let configs = from_yaml_string(text, &global_rules)
        .with_context(|| EC::ParseRule("INLINE_RULES".into()))?;
      overwrite.process_configs(configs)?
    } else {
      return Err(anyhow::anyhow!(EC::RuleNotSpecified));
    };
    Ok(Self {
      rules,
      error_count: AtomicUsize::new(0),
      max_diagnostics_shown: arg.max_results.map(usize::from),
    })
  }
}

impl Worker for ScanStdin {
  fn consume_items<P: Printer>(
    &self,
    items: Items<P::Processed>,
    mut printer: P,
  ) -> Result<ExitCode> {
    printer.before_print()?;
    for item in items {
      printer.process(item)?;
    }
    printer.after_print()?;
    let error_count = self.error_count.load(Ordering::Acquire);
    if error_count > 0 {
      Err(anyhow::anyhow!(EC::DiagnosticError(error_count)))
    } else {
      Ok(ExitCode::SUCCESS)
    }
  }
}

impl StdInWorker for ScanStdin {
  fn parse_stdin<P: Printer>(
    &self,
    src: String,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>> {
    use ast_grep_core::tree_sitter::LanguageExt;
    let Some(first_rule) = self.rules.first() else {
      return Ok(vec![]);
    };
    let lang = first_rule.language;
    let combined = CombinedScan::new(self.rules.iter().collect());
    let grep = lang.ast_grep(src);
    let path = Path::new("STDIN");
    let file_content = grep.source();
    // do not separate_fix rule in stdin mode
    let scanned = combined.scan(&grep, false);
    let mut error_count = 0usize;
    let mut diagnostic_count = 0usize;
    let mut ret = vec![];
    for (rule, matches) in scanned.matches {
      // Truncate matches if max_diagnostics_shown is set
      let matches: Vec<_> = if let Some(max) = self.max_diagnostics_shown {
        let remaining = max.saturating_sub(diagnostic_count);
        if remaining == 0 {
          break;
        }
        matches.into_iter().take(remaining).collect()
      } else {
        matches
      };
      if matches.is_empty() {
        continue;
      }
      let match_count = matches.len();
      diagnostic_count += match_count;
      if matches!(rule.severity, Severity::Error) {
        error_count = error_count.saturating_add(match_count);
      }
      let processed = match_rule_on_file(path, matches, rule, file_content, processor)?;
      ret.push(processed);
    }
    self.error_count.fetch_add(error_count, Ordering::AcqRel);
    Ok(ret)
  }
}
fn match_rule_diff_on_file<T>(
  path: &Path,
  matches: Vec<(&RuleConfig<SgLang>, NodeMatch<StrDoc<SgLang>>)>,
  processor: &impl PrintProcessor<T>,
) -> Result<T> {
  let diffs = matches
    .into_iter()
    .filter_map(|(rule, m)| {
      let fixers = &rule.fixer;
      let diff = Diff::multiple(m, &rule.matcher, fixers)?;
      Some((diff, rule))
    })
    .collect();
  let processed = processor.print_rule_diffs(diffs, path)?;
  Ok(processed)
}

fn match_rule_on_file<T>(
  path: &Path,
  matches: Vec<NodeMatch<StrDoc<SgLang>>>,
  rule: &RuleConfig<SgLang>,
  file_content: &str,
  processor: &impl PrintProcessor<T>,
) -> Result<T> {
  let file = SimpleFile::new(path.to_string_lossy(), file_content);
  let processed = if let Some(fixer) = &rule.fixer.first() {
    let diffs = matches
      .into_iter()
      .map(|m| (Diff::generate(m, &rule.matcher, fixer), rule))
      .collect();
    processor.print_rule_diffs(diffs, path)?
  } else {
    processor.print_rule(matches, file, rule)?
  };
  Ok(processed)
}

#[cfg(test)]
mod test {
  use super::*;
  use crate::custom_language::CustomLanguageAction;
  use crate::print::ColorArg;
  use std::fs::File;
  use std::io::Write;
  use tempfile::TempDir;

  const RULE: &str = r#"
id: test
message: Add your rule message here....
severity: error # error, warning, hint, info
language: Rust
rule:
  pattern: Some(123)
"#;

  // TODO: unify with verify::test
  pub fn create_test_files<'a>(
    names_and_contents: impl IntoIterator<Item = (&'a str, &'a str)>,
  ) -> TempDir {
    let dir = TempDir::new().unwrap();
    for (name, contents) in names_and_contents {
      let path = dir.path().join(name);
      let mut file = File::create(path.clone()).unwrap();
      file.write_all(contents.as_bytes()).unwrap();
      file.sync_all().unwrap();
    }
    dir
  }

  fn default_scan_arg() -> ScanArg {
    ScanArg {
      rule: None,
      inline_rules: None,
      report_style: ReportStyle::Rich,
      include_metadata: false,
      input: InputArgs {
        no_ignore: vec![],
        paths: vec![PathBuf::from(".")],
        stdin: false,
        follow: false,
        globs: vec![],
        threads: 0,
      },
      overwrite: OverwriteArgs {
        filter: None,
        min_severity: Severity::Off,
        error: None,
        warning: None,
        info: None,
        hint: None,
        off: None,
      },
      output: OutputArgs {
        interactive: false,
        json: None,
        files_with_matches: false,
        update_all: false,
        color: ColorArg::Never,
        inspect: Default::default(),
      },
      context: ContextArgs {
        before: 0,
        after: 0,
        context: 0,
      },
      format: None,
      max_results: None,
      debug: false,
    }
  }

  #[test]
  fn test_run_with_config() {
    let dir = create_test_files([("sgconfig.yml", "ruleDirs: [rules]")]);
    std::fs::create_dir_all(dir.path().join("rules")).unwrap();
    let mut file = File::create(dir.path().join("rules/test.yml")).unwrap();
    file.write_all(RULE.as_bytes()).unwrap();
    let mut file = File::create(dir.path().join("test.rs")).unwrap();
    file
      .write_all("fn test() { Some(123) }".as_bytes())
      .unwrap();
    file.sync_all().unwrap();
    let project_config = ProjectConfig::setup(
      Some(dir.path().join("sgconfig.yml")),
      CustomLanguageAction::Ignore,
    )
    .unwrap();
    let arg = default_scan_arg();
    assert!(run_with_config(arg, project_config).is_ok());
  }

  #[test]
  fn test_scan_with_inline_rules() {
    let inline_rules = "{id: test, language: ts, rule: {pattern: readFileSync}}".to_string();
    let arg = ScanArg {
      inline_rules: Some(inline_rules),
      ..default_scan_arg()
    };
    assert!(run_with_config(arg, Err(anyhow::anyhow!("not found"))).is_ok());
  }

  #[test]
  fn test_scan_with_inline_rules_diff() {
    let inline_rules =
      "{id: test, language: ts, rule: {pattern: readFileSync}, fix: 'nnn'}".to_string();
    let arg = ScanArg {
      inline_rules: Some(inline_rules),
      ..default_scan_arg()
    };
    assert!(run_with_config(arg, Err(anyhow::anyhow!("not found"))).is_ok());
  }

  // baseline test for coverage
  #[test]
  fn test_scan_with_inline_rules_error() {
    let inline_rules = "nonsense".to_string();
    let arg = ScanArg {
      inline_rules: Some(inline_rules),
      ..default_scan_arg()
    };
    let err = run_with_config(arg, Err(anyhow::anyhow!("not found"))).expect_err("should error");
    assert!(err.is::<EC>());
    assert_eq!(err.to_string(), "Cannot parse rule INLINE_RULES");
  }
}
