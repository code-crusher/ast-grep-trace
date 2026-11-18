use crate::lang::SgLang;
use ast_grep_config::{LabelStyle, Metadata, RuleConfig, Severity};
use ast_grep_core::Doc;
use ast_grep_core::{Node as SgNode, meta_var::MetaVariable, tree_sitter::StrDoc};

type Node<'t, L> = SgNode<'t, StrDoc<L>>;

use std::collections::HashMap;

use super::{Diff, NodeMatch, PrintProcessor, Printer};
use anyhow::Result;
use clap::ValueEnum;
use codespan_reporting::files::SimpleFile;
use serde::{Deserialize, Serialize};

use std::borrow::Cow;
use std::io::{Stdout, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// Events of the ripgrep JSON format, see
/// https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md#json
/// Each event is serialized as one JSON object per line.
#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "lowercase")]
enum RgEvent<'a> {
  Begin(RgBegin<'a>),
  Match(RgMatchData<'a>),
  Context(RgMatchData<'a>),
  End(RgEnd<'a>),
  Summary(RgSummary),
}

#[derive(Serialize)]
struct RgBegin<'a> {
  path: RgText<'a>,
}

#[derive(Serialize)]
struct RgText<'a> {
  text: Cow<'a, str>,
}

/// Data of a `match`/`context` event. `language` is an ast-grep extension
/// and is absent in context records, mirroring ripgrep's own shape.
#[derive(Serialize)]
struct RgMatchData<'a> {
  path: RgText<'a>,
  lines: RgText<'a>,
  /// one-based line number of the first line in `lines`
  line_number: usize,
  /// byte offset of the first line in `lines`
  absolute_offset: usize,
  submatches: Vec<RgSubMatch<'a>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  language: Option<SgLang>,
}

#[derive(Serialize)]
struct RgSubMatch<'a> {
  r#match: RgText<'a>,
  /// byte offsets relative to the start of `lines`
  start: usize,
  end: usize,
}

#[derive(Serialize)]
struct RgEnd<'a> {
  path: RgText<'a>,
  binary_offset: Option<usize>,
  stats: RgStats,
}

#[derive(Serialize)]
struct RgStats {
  elapsed: RgDuration,
  searches: usize,
  searches_with_match: usize,
  bytes_searched: usize,
  bytes_printed: usize,
  matched_lines: usize,
  matches: usize,
}

#[derive(Serialize)]
struct RgDuration {
  secs: u64,
  nanos: u32,
  human: String,
}

impl RgDuration {
  fn new(duration: Duration) -> Self {
    Self {
      secs: duration.as_secs(),
      nanos: duration.subsec_nanos(),
      human: format!("{:.6}s", duration.as_secs_f64()),
    }
  }
}

#[derive(Serialize)]
struct RgSummary {
  elapsed_total: RgDuration,
  stats: RgStats,
}

fn rg_duration_zero() -> RgDuration {
  RgDuration::new(Duration::ZERO)
}

/// Byte offset of the start of every line in a source file.
struct LineIndex<'s> {
  source: &'s str,
  starts: Vec<usize>,
}

impl<'s> LineIndex<'s> {
  fn new(source: &'s str) -> Self {
    let bytes = source.as_bytes();
    // the start of a line after the final newline is a phantom line, skip it
    let mut starts = vec![0];
    for (i, &b) in bytes.iter().enumerate() {
      if b == b'\n' && i + 1 < bytes.len() {
        starts.push(i + 1);
      }
    }
    Self { source, starts }
  }

  fn line_count(&self) -> usize {
    self.starts.len()
  }

  /// zero-based index of the line containing the byte `offset`
  fn line_of(&self, offset: usize) -> usize {
    match self.starts.binary_search(&offset) {
      Ok(i) => i,
      Err(i) => i - 1,
    }
  }

  fn line_start(&self, line: usize) -> usize {
    self.starts[line]
  }

  /// text of the line, including its trailing newline if present
  fn line_text(&self, line: usize) -> &'s str {
    let start = self.starts[line];
    let end = self
      .starts
      .get(line + 1)
      .copied()
      .unwrap_or(self.source.len());
    &self.source[start..end]
  }

  /// text spanning lines `from`..=`to`, including trailing newline if present
  fn span_text(&self, from: usize, to: usize) -> &'s str {
    let start = self.starts[from];
    let end = self
      .starts
      .get(to + 1)
      .copied()
      .unwrap_or(self.source.len());
    &self.source[start..end]
  }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Zero-based character position in a file.
struct Position {
  /// Zero-based line number
  line: usize,
  /// Zero-based character column in a line
  column: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Range {
  /// inclusive start, exclusive end
  byte_offset: std::ops::Range<usize>,
  start: Position,
  end: Position,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchNode<'t> {
  text: Cow<'t, str>,
  range: Range,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchLabel<'t, 'r> {
  text: Cow<'t, str>,
  range: Range,
  #[serde(skip_serializing_if = "Option::is_none")]
  message: Option<&'r str>,
  style: LabelStyle,
}

/// a sub field of leading and trailing text count around match.
/// plugin authors can use it to split `lines` into leading, matching and trailing
/// See ast-grep/ast-grep#1381
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CharCount {
  leading: usize,
  trailing: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchJSON<'t, 'b> {
  text: Cow<'t, str>,
  range: Range,
  file: Cow<'b, str>,
  lines: String,
  char_count: CharCount,
  #[serde(skip_serializing_if = "Option::is_none")]
  replacement: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  replacement_offsets: Option<std::ops::Range<usize>>,
  language: SgLang,
  #[serde(skip_serializing_if = "Option::is_none")]
  meta_variables: Option<MetaVariables<'t>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetaVariables<'t> {
  single: HashMap<String, MatchNode<'t>>,
  multi: HashMap<String, Vec<MatchNode<'t>>>,
  transformed: HashMap<String, String>,
}
fn from_env<'t>(nm: &NodeMatch<'t>) -> Option<MetaVariables<'t>> {
  let env = nm.get_env();
  let mut vars = env.get_matched_variables().peekable();
  vars.peek()?;
  let mut single = HashMap::new();
  let mut multi = HashMap::new();
  let mut transformed = HashMap::new();
  for var in vars {
    use MetaVariable as MV;
    match var {
      MV::Capture(n, _) => {
        if let Some(node) = env.get_match(&n) {
          single.insert(
            n,
            MatchNode {
              text: node.text(),
              range: get_range(node),
            },
          );
        } else if let Some(bytes) = env.get_transformed(&n) {
          transformed.insert(n, String::from_utf8_lossy(bytes).into_owned());
        }
      }
      MV::MultiCapture(n) => {
        let nodes = env.get_multiple_matches(&n);
        multi.insert(
          n,
          nodes
            .into_iter()
            .map(|node| MatchNode {
              text: node.text(),
              range: get_range(&node),
            })
            .collect(),
        );
      }
      _ => continue,
    }
  }
  Some(MetaVariables {
    single,
    multi,
    transformed,
  })
}

fn get_range(n: &Node<'_, SgLang>) -> Range {
  let start_pos = n.start_pos();
  let end_pos = n.end_pos();
  Range {
    byte_offset: n.range(),
    start: Position {
      line: start_pos.line(),
      column: start_pos.column(n),
    },
    end: Position {
      line: end_pos.line(),
      column: end_pos.column(n),
    },
  }
}

impl<'t, 'b> MatchJSON<'t, 'b> {
  fn new(nm: NodeMatch<'t>, path: &'b str, context: (u16, u16)) -> Self {
    let display = nm.display_context(context.0 as usize, context.1 as usize);
    let lines = format!("{}{}{}", display.leading, display.matched, display.trailing);
    MatchJSON {
      file: Cow::Borrowed(path),
      text: nm.text(),
      lines,
      char_count: CharCount {
        leading: display.leading.chars().count(),
        trailing: display.trailing.chars().count(),
      },
      language: *nm.lang(),
      replacement: None,
      replacement_offsets: None,
      range: get_range(&nm),
      meta_variables: from_env(&nm),
    }
  }

  fn diff(diff: Diff<'t>, path: &'b str, context: (u16, u16)) -> Self {
    let mut ret = Self::new(diff.node_match, path, context);
    ret.replacement = Some(diff.replacement);
    ret.replacement_offsets = Some(diff.range);
    ret
  }
}
fn get_labels<'b, 't>(rule: &'b RuleConfig<SgLang>, nm: &NodeMatch<'t>) -> Vec<MatchLabel<'t, 'b>> {
  rule
    .get_labels(nm)
    .into_iter()
    .map(|label| {
      let start_pos = label.start_node.start_pos();
      let end_pos = label.end_node.end_pos();
      let byte_offset = label.range();
      let range = Range {
        byte_offset: byte_offset.clone(),
        start: Position {
          line: start_pos.line(),
          // TODO:using pos.column with non-matching node is Okay now
          // because it only uses the source of the root doc
          column: start_pos.column(nm),
        },
        end: Position {
          line: end_pos.line(),
          column: end_pos.column(nm),
        },
      };
      let source = nm.get_doc().get_source();
      MatchLabel {
        text: Cow::Borrowed(&source[byte_offset]),
        range,
        message: label.message,
        style: label.style,
      }
    })
    .collect()
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleMatchJSON<'t, 'b> {
  #[serde(flatten)]
  matched: MatchJSON<'t, 'b>,
  rule_id: &'b str,
  severity: Severity,
  note: Option<String>,
  message: String,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  labels: Vec<MatchLabel<'t, 'b>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  metadata: Option<Cow<'b, Metadata>>,
}
impl<'t, 'b> RuleMatchJSON<'t, 'b> {
  fn new(nm: NodeMatch<'t>, path: &'b str, rule: &'b RuleConfig<SgLang>, metadata: bool) -> Self {
    let message = rule.get_message(&nm);
    let labels = get_labels(rule, &nm);
    let matched = MatchJSON::new(nm, path, (0, 0));
    let metadata = if metadata {
      rule.metadata.as_ref().map(Cow::Borrowed)
    } else {
      None
    };
    Self {
      matched,
      rule_id: &rule.id,
      severity: rule.severity.clone(),
      note: rule.note.clone(),
      message,
      labels,
      metadata,
    }
  }
  fn diff(diff: Diff<'t>, path: &'b str, rule: &'b RuleConfig<SgLang>, metadata: bool) -> Self {
    let nm = &diff.node_match;
    let message = rule.get_message(nm);
    let labels = get_labels(rule, nm);
    let matched = MatchJSON::diff(diff, path, (0, 0));
    let metadata = if metadata {
      rule.metadata.as_ref().map(Cow::Borrowed)
    } else {
      None
    };
    Self {
      matched,
      rule_id: &rule.id,
      severity: rule.severity.clone(),
      note: rule.note.clone(),
      message,
      labels,
      metadata,
    }
  }
}

/// Controls how to print and format JSON object in output.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum JsonStyle {
  /// Prints the matches as a pretty-printed JSON array, with indentation and line breaks.
  /// This is useful for human readability, but not for parsing by other programs.
  /// This is the default value for the `--json` option.
  Pretty,
  /// Prints each match as a separate JSON object, followed by a newline character.
  /// This is useful for streaming the output to other programs that can read one object per line.
  Stream,
  /// Prints the matches as a single-line JSON array, without any whitespace.
  /// This is useful for saving space and minimizing the output size.
  Compact,
  /// Prints the matches as an event stream in ripgrep's JSON format.
  ///
  /// This emits `begin`, `match`, `context`, `end` and `summary` events, compatible
  /// with tools that consume `rg --json` output (e.g. delta). Context lines from
  /// `-A`/`-B`/`-C` are retained, and matches on the same line are coalesced into
  /// one `match` event with multiple `submatches`, just like ripgrep.
  Ripgrep,
}

pub struct JSONPrinter<W: Write> {
  output: W,
  style: JsonStyle,
  context: (u16, u16),
  include_metadata: bool,
  // indicate if any matches happened
  matched: bool,
  // ripgrep style bookkeeping
  rg_start: Option<Instant>,
  // path of the file whose begin/end events are open
  rg_path: Option<String>,
  rg_file_count: usize,
  // stats of the currently open file
  rg_file: (usize, usize),
  // total stats: files, matched_lines, matches
  rg_total: (usize, usize, usize),
}
impl JSONPrinter<Stdout> {
  pub fn stdout(style: JsonStyle) -> Self {
    Self::new(std::io::stdout(), style)
  }
}

impl<W: Write> JSONPrinter<W> {
  pub fn new(output: W, style: JsonStyle) -> Self {
    // no match happened yet
    Self {
      style,
      output,
      include_metadata: false,
      context: (0, 0),
      matched: false,
      rg_start: None,
      rg_path: None,
      rg_file_count: 0,
      rg_file: (0, 0),
      rg_total: (0, 0, 0),
    }
  }

  pub fn context(mut self, context: (u16, u16)) -> Self {
    self.context = context;
    self
  }

  pub fn include_metadata(mut self, include: bool) -> Self {
    self.include_metadata = include;
    self
  }

  fn write_event(&mut self, event: &RgEvent<'_>) -> Result<()> {
    serde_json::to_writer(&mut self.output, event)?;
    writeln!(self.output)?;
    Ok(())
  }

  /// Emit the `end` event of the currently open file, if any.
  fn close_ripgrep_file(&mut self) -> Result<()> {
    let Some(path) = self.rg_path.take() else {
      return Ok(());
    };
    let (matched_lines, matches) = self.rg_file;
    let event = RgEvent::End(RgEnd {
      path: RgText {
        text: Cow::Owned(path),
      },
      binary_offset: None,
      stats: RgStats {
        elapsed: rg_duration_zero(),
        searches: 1,
        searches_with_match: 1,
        bytes_searched: 0,
        bytes_printed: 0,
        matched_lines,
        matches,
      },
    });
    self.write_event(&event)
  }

  fn process_ripgrep(&mut self, buf: RipgrepBuffer) -> Result<()> {
    if buf.body.is_empty() {
      return Ok(());
    }
    if self.rg_path.as_deref() != Some(buf.path.as_str()) {
      self.close_ripgrep_file()?;
      let event = RgEvent::Begin(RgBegin {
        path: RgText {
          text: Cow::Borrowed(&buf.path),
        },
      });
      self.write_event(&event)?;
      self.rg_path = Some(buf.path.clone());
      self.rg_file = (0, 0);
      self.rg_file_count += 1;
      self.rg_total.0 += 1;
    }
    self.output.write_all(&buf.body)?;
    self.rg_file.0 += buf.matched_lines;
    self.rg_file.1 += buf.matches;
    self.rg_total.1 += buf.matched_lines;
    self.rg_total.2 += buf.matches;
    Ok(())
  }
}

impl<W: Write> Printer for JSONPrinter<W> {
  type Processed = Buffer;
  type Processor = JSONProcessor;

  fn get_processor(&self) -> JSONProcessor {
    JSONProcessor {
      style: self.style,
      context: self.context,
      include_metadata: self.include_metadata,
    }
  }
  fn process(&mut self, processed: Buffer) -> Result<()> {
    match processed {
      Buffer::Json(bytes) => {
        if bytes.is_empty() {
          return Ok(());
        }
        let output = &mut self.output;
        let matched = self.matched;
        self.matched = true;
        // print separator if there was a match before
        if matched {
          let separator = match self.style {
            JsonStyle::Pretty => ",\n",
            JsonStyle::Stream => "",
            JsonStyle::Compact => ",",
            JsonStyle::Ripgrep => "",
          };
          write!(output, "{separator}")?;
        } else if self.style == JsonStyle::Pretty {
          // print newline for the first match in pretty style
          writeln!(output)?;
        }
        output.write_all(&bytes)?;
      }
      Buffer::Ripgrep(buf) => {
        self.matched = true;
        self.process_ripgrep(buf)?;
      }
    }
    Ok(())
  }

  fn before_print(&mut self) -> Result<()> {
    if self.style == JsonStyle::Stream {
      return Ok(());
    }
    if self.style == JsonStyle::Ripgrep {
      self.rg_start = Some(Instant::now());
      return Ok(());
    }
    write!(self.output, "[")?;
    Ok(())
  }

  fn after_print(&mut self) -> Result<()> {
    if self.style == JsonStyle::Stream {
      return Ok(());
    }
    if self.style == JsonStyle::Ripgrep {
      self.close_ripgrep_file()?;
      let elapsed = self
        .rg_start
        .map(|start| start.elapsed())
        .unwrap_or_default();
      let (files, matched_lines, matches) = self.rg_total;
      let event = RgEvent::Summary(RgSummary {
        elapsed_total: RgDuration::new(elapsed),
        stats: RgStats {
          elapsed: RgDuration::new(elapsed),
          searches: files,
          searches_with_match: files,
          bytes_searched: 0,
          bytes_printed: 0,
          matched_lines,
          matches,
        },
      });
      return self.write_event(&event);
    }
    let output = &mut self.output;
    if self.matched && self.style == JsonStyle::Pretty {
      writeln!(output)?;
    }
    writeln!(output, "]")?;
    Ok(())
  }
}

pub struct JSONProcessor {
  style: JsonStyle,
  include_metadata: bool,
  context: (u16, u16),
}

/// A coalesced ripgrep `match` event: all matches whose line spans touch
/// share one record, like multiple submatches on one line in ripgrep.
struct RgRecord<'a> {
  start_line: usize,
  end_line: usize,
  submatches: Vec<(usize, usize, &'a str)>,
}

/// Body of one file's ripgrep events, produced by the processor and framed
/// with begin/end by the printer.
pub struct RipgrepBuffer {
  path: String,
  body: Vec<u8>,
  matched_lines: usize,
  matches: usize,
}

impl Buffer {
  fn empty_ripgrep(path: &str) -> Self {
    Self::Ripgrep(RipgrepBuffer {
      path: path.to_string(),
      body: Vec::new(),
      matched_lines: 0,
      matches: 0,
    })
  }
}

impl JSONProcessor {
  fn print_docs<S: Serialize>(&self, mut docs: impl Iterator<Item = S>) -> Result<Buffer> {
    let mut ret = Vec::new();
    let Some(doc) = docs.next() else {
      return Ok(Buffer::Json(ret));
    };
    let output = &mut ret;
    match self.style {
      JsonStyle::Pretty => {
        serde_json::to_writer_pretty(&mut *output, &doc)?;
        for doc in docs {
          writeln!(&mut *output, ",")?;
          serde_json::to_writer_pretty(&mut *output, &doc)?;
        }
      }
      JsonStyle::Stream => {
        // Stream mode requires a newline after each object
        for doc in std::iter::once(doc).chain(docs) {
          serde_json::to_writer(&mut *output, &doc)?;
          writeln!(&mut *output)?;
        }
      }
      JsonStyle::Compact => {
        serde_json::to_writer(&mut *output, &doc)?;
        for doc in docs {
          write!(output, ",")?;
          serde_json::to_writer(&mut *output, &doc)?;
        }
      }
      JsonStyle::Ripgrep => unreachable!("ripgrep style does not print JSON docs"),
    }
    Ok(Buffer::Json(ret))
  }

  /// Build all ripgrep events of one file from its matches.
  /// `matches` must come from a single document, in source order.
  fn ripgrep_events<'a, 'm>(&self, matches: &[&'m NodeMatch<'a>], path: &str) -> Buffer {
    let Some(first) = matches.first() else {
      return Buffer::empty_ripgrep(path);
    };
    let source = first.root().get_text();
    let language = *first.lang();
    let index = LineIndex::new(source);
    let (before, after) = (self.context.0 as usize, self.context.1 as usize);
    let last_line = index.line_count().saturating_sub(1);

    // skip overlapping matches, they are nested matches in pre-order
    let mut records: Vec<RgRecord> = vec![];
    let mut last_end = 0;
    for nm in matches.iter().copied() {
      let range = nm.range();
      if range.start < last_end {
        continue;
      }
      let end = range.end.min(source.len());
      let start_line = index.line_of(range.start);
      let end_line = if end > range.start {
        index.line_of(end - 1)
      } else {
        start_line
      };
      let text = &source[range.start..end];
      last_end = range.end;
      // matches on the same line coalesce into one record with many submatches
      if records.last_mut().is_some_and(|rec| start_line <= rec.end_line) {
        let rec = records.last_mut().expect("just checked");
        rec.end_line = rec.end_line.max(end_line);
        rec.submatches.push((range.start, end, text));
      } else {
        records.push(RgRecord {
          start_line,
          end_line,
          submatches: vec![(range.start, end, text)],
        });
      }
    }

    let mut body = Vec::new();
    let mut matched_lines = 0;
    let mut match_count = 0;
    let emit = |event: &RgEvent<'_>, body: &mut Vec<u8>| {
      serde_json::to_writer(&mut *body, event).expect("writing to Vec cannot fail");
      body.push(b'\n');
    };
    let mut record_idx = 0;
    while record_idx < records.len() {
      // merge overlapping/adjacent context windows of consecutive records
      let first = &records[record_idx];
      let window_start = first.start_line.saturating_sub(before);
      let mut window_end = (first.end_line + after).min(last_line);
      let mut group_end = record_idx;
      let mut next = record_idx + 1;
      while let Some(rec) = records.get(next)
        && rec.start_line.saturating_sub(before) <= window_end + 1
      {
        window_end = window_end.max((rec.end_line + after).min(last_line));
        group_end = next;
        next += 1;
      }
      // walk the window line by line, emitting records and context lines
      let mut line = window_start;
      for rec in &records[record_idx..=group_end] {
        for l in line..rec.start_line {
          let event = RgEvent::Context(rg_context(&index, path, l));
          emit(&event, &mut body);
        }
        let event = RgEvent::Match(rg_match(&index, path, rec, language));
        emit(&event, &mut body);
        line = rec.end_line + 1;
        matched_lines += rec.end_line - rec.start_line + 1;
        match_count += rec.submatches.len();
      }
      for l in line..=window_end {
        let event = RgEvent::Context(rg_context(&index, path, l));
        emit(&event, &mut body);
      }
      record_idx = group_end + 1;
    }
    Buffer::Ripgrep(RipgrepBuffer {
      path: path.to_string(),
      body,
      matched_lines,
      matches: match_count,
    })
  }
}

fn rg_text<'a>(text: impl Into<Cow<'a, str>>) -> RgText<'a> {
  RgText { text: text.into() }
}

fn rg_context<'s>(index: &LineIndex<'s>, path: &'s str, line: usize) -> RgMatchData<'s> {
  RgMatchData {
    path: rg_text(path),
    lines: rg_text(index.line_text(line)),
    line_number: line + 1,
    absolute_offset: index.line_start(line),
    submatches: vec![],
    language: None,
  }
}

fn rg_match<'s>(
  index: &LineIndex<'s>,
  path: &'s str,
  rec: &RgRecord<'s>,
  language: SgLang,
) -> RgMatchData<'s> {
  let absolute_offset = index.line_start(rec.start_line);
  RgMatchData {
    path: rg_text(path),
    lines: rg_text(index.span_text(rec.start_line, rec.end_line)),
    line_number: rec.start_line + 1,
    absolute_offset,
    submatches: rec
      .submatches
      .iter()
      .map(|&(start, end, text)| RgSubMatch {
        r#match: rg_text(text),
        start: start - absolute_offset,
        end: end - absolute_offset,
      })
      .collect(),
    language: Some(language),
  }
}

pub enum Buffer {
  Json(Vec<u8>),
  Ripgrep(RipgrepBuffer),
}

impl PrintProcessor<Buffer> for JSONProcessor {
  fn print_rule(
    &self,
    matches: Vec<NodeMatch>,
    file: SimpleFile<Cow<str>, &str>,
    rule: &RuleConfig<SgLang>,
  ) -> Result<Buffer> {
    let path = file.name();
    if self.style == JsonStyle::Ripgrep {
      return Ok(self.ripgrep_events(&matches.iter().collect::<Vec<_>>(), path));
    }
    let jsons = matches
      .into_iter()
      .map(|nm| RuleMatchJSON::new(nm, path, rule, self.include_metadata));
    self.print_docs(jsons)
  }

  fn print_matches(&self, matches: Vec<NodeMatch>, path: &Path) -> Result<Buffer> {
    let path = path.to_string_lossy();
    if self.style == JsonStyle::Ripgrep {
      return Ok(self.ripgrep_events(&matches.iter().collect::<Vec<_>>(), &path));
    }
    let context = self.context;
    let jsons = matches
      .into_iter()
      .map(|nm| MatchJSON::new(nm, &path, context));
    self.print_docs(jsons)
  }

  fn print_diffs(&self, diffs: Vec<Diff>, path: &Path) -> Result<Buffer> {
    let path = path.to_string_lossy();
    if self.style == JsonStyle::Ripgrep {
      // ripgrep has no notion of rewrite, report the matched nodes instead
      let matches = diffs.iter().map(|d| &d.node_match).collect::<Vec<_>>();
      return Ok(self.ripgrep_events(&matches, &path));
    }
    let context = self.context;
    let jsons = diffs
      .into_iter()
      .map(|diff| MatchJSON::diff(diff, &path, context));
    self.print_docs(jsons)
  }
  fn print_rule_diffs(
    &self,
    diffs: Vec<(Diff, &RuleConfig<SgLang>)>,
    path: &Path,
  ) -> Result<Buffer> {
    let path = path.to_string_lossy();
    if self.style == JsonStyle::Ripgrep {
      // ripgrep has no notion of rewrite, report the matched nodes instead
      let matches = diffs.iter().map(|d| &d.0.node_match).collect::<Vec<_>>();
      return Ok(self.ripgrep_events(&matches, &path));
    }
    let jsons = diffs
      .into_iter()
      .map(|(diff, rule)| RuleMatchJSON::diff(diff, &path, rule, self.include_metadata));
    self.print_docs(jsons)
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use ast_grep_config::{Fixer, GlobalRules, from_yaml_string};
  use ast_grep_language::{LanguageExt, SupportLang};

  struct Test(String);
  impl Write for Test {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
      let s = std::str::from_utf8(buf).expect("should ok");
      self.0.push_str(s);
      Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }
  fn make_test_printer(style: JsonStyle) -> JSONPrinter<Test> {
    JSONPrinter::new(Test(String::new()), style)
  }
  fn get_text(printer: &JSONPrinter<Test>) -> String {
    let output = &printer.output;
    output.0.to_string()
  }

  #[test]
  fn test_empty_printer() {
    for style in [JsonStyle::Pretty, JsonStyle::Compact] {
      let mut printer = make_test_printer(style);
      printer.before_print().unwrap();
      let buffer = printer
        .get_processor()
        .print_matches(vec![], "test.tsx".as_ref())
        .unwrap();
      printer.process(buffer).unwrap();
      printer.after_print().unwrap();
      assert_eq!(get_text(&printer), "[]\n");
    }
  }

  // source, pattern, replace, debug note
  type Case<'t> = (&'t str, &'t str, &'t str, &'t str);

  const MATCHES_CASES: &[Case] = &[
    ("let a = 123", "a", "b", "Simple match"),
    (
      "Some(1), Some(2), Some(3)",
      "Some",
      "Any",
      "Same line match",
    ),
    (
      "Some(1), Some(2)\nSome(3), Some(4)",
      "Some",
      "Any",
      "Multiple line match",
    ),
    (
      "import a from 'b';import a from 'b';",
      "import a from 'b';",
      "import c from 'b';",
      "immediate following but not overlapping",
    ),
  ];

  #[test]
  fn test_invariant() {
    for &(source, pattern, _, note) in MATCHES_CASES {
      // heading is required for CI
      let mut printer = make_test_printer(JsonStyle::Pretty);
      let grep = SgLang::from(SupportLang::Tsx).ast_grep(source);
      let matches = grep.root().find_all(pattern).collect();
      printer.before_print().unwrap();
      let buffer = printer
        .get_processor()
        .print_matches(matches, "test.tsx".as_ref())
        .unwrap();
      printer.process(buffer).unwrap();
      printer.after_print().unwrap();
      let json_str = get_text(&printer);
      let json: Vec<MatchJSON> = serde_json::from_str(&json_str).unwrap();
      assert_eq!(json[0].text, pattern, "{note}");
    }
  }

  #[test]
  fn test_replace_json() {
    for &(source, pattern, replace, note) in MATCHES_CASES {
      // heading is required for CI
      let mut printer = make_test_printer(JsonStyle::Compact);
      let lang = SgLang::from(SupportLang::Tsx);
      let grep = lang.ast_grep(source);
      let root = grep.root();
      let matches = root.find_all(pattern);
      let fixer = Fixer::from_str(replace, &lang).expect("should work");
      let diffs = matches
        .map(|m| Diff::generate(m, &pattern, &fixer))
        .collect();
      printer.before_print().unwrap();
      let buffer = printer
        .get_processor()
        .print_diffs(diffs, "test.tsx".as_ref())
        .unwrap();
      printer.process(buffer).unwrap();
      printer.after_print().unwrap();
      let json_str = get_text(&printer);
      let json: Vec<MatchJSON> = serde_json::from_str(&json_str).unwrap();
      let actual = json[0].replacement.as_ref().expect("should have diff");
      assert_eq!(actual, replace, "{note}");
    }
  }

  fn make_rule(rule: &str) -> RuleConfig<SgLang> {
    let globals = GlobalRules::default();
    from_yaml_string(
      &format!(
        r#"
id: test
message: test rule
severity: info
language: TypeScript
note: a long random note
rule:
  pattern: "{rule}""#
      ),
      &globals,
    )
    .unwrap()
    .pop()
    .unwrap()
  }

  #[test]
  fn test_rule_json() {
    for &(source, pattern, _, note) in MATCHES_CASES {
      // TODO: understand why import does not work
      if source.contains("import") {
        continue;
      }
      let mut printer = make_test_printer(JsonStyle::Pretty);
      let grep = SgLang::from(SupportLang::Tsx).ast_grep(source);
      let rule = make_rule(pattern);
      let matches = grep.root().find_all(&rule.matcher).collect();
      printer.before_print().unwrap();
      let file = SimpleFile::new(Cow::Borrowed("test.ts"), source);
      let buffer = printer
        .get_processor()
        .print_rule(matches, file, &rule)
        .unwrap();
      printer.process(buffer).unwrap();
      printer.after_print().unwrap();
      let json_str = get_text(&printer);
      let json: Vec<RuleMatchJSON> = serde_json::from_str(&json_str).unwrap();
      assert_eq!(json[0].matched.text, pattern, "{note}");
      assert_eq!(json[0].note, rule.note);
    }
  }

  #[test]
  fn test_single_matched_json() {
    let mut printer = make_test_printer(JsonStyle::Pretty);
    let lang = SgLang::from(SupportLang::Tsx);
    let grep = lang.ast_grep("console.log(123)");
    let matches = grep.root().find_all("console.log($A)").collect();
    printer.before_print().unwrap();
    let buffer = printer
      .get_processor()
      .print_matches(matches, "test.tsx".as_ref())
      .unwrap();
    printer.process(buffer).unwrap();
    printer.after_print().unwrap();
    let json_str = get_text(&printer);
    let json: Vec<MatchJSON> = serde_json::from_str(&json_str).unwrap();
    let actual = &json[0]
      .meta_variables
      .as_ref()
      .expect("should exist")
      .single;
    assert_eq!(actual["A"].text, "123");
  }

  #[test]
  fn test_multi_matched_json() {
    let mut printer = make_test_printer(JsonStyle::Compact);
    let lang = SgLang::from(SupportLang::Tsx);
    let grep = lang.ast_grep("console.log(1, 2, 3)");
    let matches = grep.root().find_all("console.log($$$A)").collect();
    printer.before_print().unwrap();
    let buffer = printer
      .get_processor()
      .print_matches(matches, "test.tsx".as_ref())
      .unwrap();
    printer.process(buffer).unwrap();
    printer.after_print().unwrap();
    let json_str = get_text(&printer);
    let json: Vec<MatchJSON> = serde_json::from_str(&json_str).unwrap();
    let actual = &json[0].meta_variables.as_ref().expect("should exist").multi;
    assert_eq!(actual["A"][0].text, "1");
    assert_eq!(actual["A"][2].text, "2");
    assert_eq!(actual["A"][4].text, "3");
  }

  #[test]
  fn test_streaming() {
    for &(source, pattern, _, note) in MATCHES_CASES {
      let mut printer = make_test_printer(JsonStyle::Stream);
      let grep = SgLang::from(SupportLang::Tsx).ast_grep(source);
      let matches = grep.root().find_all(pattern).collect();
      printer.before_print().unwrap();
      let buffer = printer
        .get_processor()
        .print_matches(matches, "test.tsx".as_ref())
        .unwrap();
      printer.process(buffer).unwrap();
      printer.after_print().unwrap();
      let json_str = get_text(&printer);
      let jsons: Vec<&str> = json_str.lines().collect();
      assert!(!jsons.is_empty());
      let json: Vec<MatchJSON> = jsons
        .into_iter()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
      assert_eq!(json[0].text, pattern, "{note}");
    }
  }

  /// helper: run ripgrep-style printing of `source` and return the parsed events
  fn ripgrep_events_of(
    source: &str,
    pattern: &str,
    context: (u16, u16),
  ) -> Vec<serde_json::Value> {
    let mut printer = make_test_printer(JsonStyle::Ripgrep).context(context);
    let grep = SgLang::from(SupportLang::Tsx).ast_grep(source);
    let matches = grep.root().find_all(pattern).collect();
    printer.before_print().unwrap();
    let buffer = printer
      .get_processor()
      .print_matches(matches, "test.tsx".as_ref())
      .unwrap();
    printer.process(buffer).unwrap();
    printer.after_print().unwrap();
    get_text(&printer)
      .lines()
      .map(|l| serde_json::from_str(l).unwrap())
      .collect()
  }

  fn event_types(events: &[serde_json::Value]) -> Vec<&str> {
    events.iter().map(|e| e["type"].as_str().unwrap()).collect()
  }

  #[test]
  fn test_ripgrep_same_line_coalescing() {
    // matches on the same line share one match event with several submatches
    let events = ripgrep_events_of("bbb bbb bbb", "bbb", (0, 0));
    assert_eq!(event_types(&events), ["begin", "match", "end", "summary"]);
    let data = &events[1]["data"];
    assert_eq!(data["lines"]["text"], "bbb bbb bbb");
    assert_eq!(data["line_number"], 1);
    assert_eq!(data["absolute_offset"], 0);
    let submatches = data["submatches"].as_array().unwrap();
    assert_eq!(submatches.len(), 3);
    assert_eq!(submatches[0]["start"], 0);
    assert_eq!(submatches[2]["end"], 11);
    assert_eq!(events[2]["data"]["stats"]["matches"], 3);
    assert_eq!(events[3]["data"]["stats"]["matched_lines"], 1);
  }

  #[test]
  fn test_ripgrep_context_lines() {
    // context lines are retained and deduplicated between close matches
    let source = "aaa\nbbb\nccc\nddd\neee\nfff bbb\nggg\n";
    let events = ripgrep_events_of(source, "bbb", (1, 1));
    assert_eq!(
      event_types(&events),
      ["begin", "context", "match", "context", "context", "match", "context", "end", "summary"]
    );
    assert_eq!(events[1]["data"]["lines"]["text"], "aaa\n");
    assert_eq!(events[1]["data"]["line_number"], 1);
    assert_eq!(events[4]["data"]["lines"]["text"], "eee\n");
    assert_eq!(events[5]["data"]["submatches"][0]["start"], 4);
    assert!(events[5]["data"].get("language").is_some());
    // context events have no submatches and no language
    assert_eq!(events[1]["data"]["submatches"].as_array().unwrap().len(), 0);
    assert!(events[1]["data"].get("language").is_none());
  }

  #[test]
  fn test_ripgrep_multi_line_match() {
    let source = "function foo() {\n  bar();\n}\n";
    let events = ripgrep_events_of(source, "function $F() { $$$ }", (0, 0));
    let data = &events[1]["data"];
    // a multi-line match spans its lines in one event, like rg -U
    assert_eq!(data["lines"]["text"], "function foo() {\n  bar();\n}\n");
    assert_eq!(data["line_number"], 1);
    assert_eq!(events[2]["data"]["stats"]["matched_lines"], 3);
  }

  #[test]
  fn test_ripgrep_no_match() {
    let events = ripgrep_events_of("aaa", "zzz", (0, 0));
    // no file had a match, only the summary is emitted
    assert_eq!(event_types(&events), ["summary"]);
    assert_eq!(events[0]["data"]["stats"]["matches"], 0);
  }

  #[test]
  fn test_ripgrep_multi_file_framing() {
    // two files produce begin/end per file and one summary
    let mut printer = make_test_printer(JsonStyle::Ripgrep);
    printer.before_print().unwrap();
    for (path, source) in [("a.tsx", "let a = 1"), ("b.tsx", "let b = 2")] {
      let grep = SgLang::from(SupportLang::Tsx).ast_grep(source);
      let matches = grep.root().find_all("let $A = $B").collect();
      let buffer = printer
        .get_processor()
        .print_matches(matches, path.as_ref())
        .unwrap();
      printer.process(buffer).unwrap();
    }
    printer.after_print().unwrap();
    let events: Vec<serde_json::Value> = get_text(&printer)
      .lines()
      .map(|l| serde_json::from_str(l).unwrap())
      .collect();
    assert_eq!(
      event_types(&events),
      ["begin", "match", "end", "begin", "match", "end", "summary"]
    );
    assert_eq!(events[6]["data"]["stats"]["searches"], 2);
    assert_eq!(events[6]["data"]["stats"]["searches_with_match"], 2);
  }

  use crate::verify::test::get_rule_config;
  const TRANSFORM_TEXT: &str = "
transform:
  B:
    substring:
      source: $A
      startChar: 1
      endChar: -1
";
  #[test]
  fn test_transform() {
    let mut printer = make_test_printer(JsonStyle::Compact);
    let rule = get_rule_config(&format!("pattern: console.log($A)\n{TRANSFORM_TEXT}"));
    let grep = SgLang::from(SupportLang::TypeScript).ast_grep("console.log(123)");
    let matches = grep.root().find_all(&rule.matcher).collect();
    printer.before_print().unwrap();
    let buffer = printer
      .get_processor()
      .print_matches(matches, "test.tsx".as_ref())
      .unwrap();
    printer.process(buffer).unwrap();
    printer.after_print().unwrap();
    let json_str = get_text(&printer);
    let json: Vec<MatchJSON> = serde_json::from_str(&json_str).unwrap();
    let metas = &json[0].meta_variables.as_ref().expect("should exist");
    assert_eq!(metas.single["A"].text, "123");
    assert_eq!(metas.transformed["B"], "2");
  }

  const LABEL_TEXT: &str = "
labels:
  A:
    style: primary
    message: var label
";
  #[test]
  fn test_label() {
    let mut printer = make_test_printer(JsonStyle::Compact);
    let rule = get_rule_config(&format!("pattern: console.log($A)\n{LABEL_TEXT}"));
    let grep = SgLang::from(SupportLang::TypeScript).ast_grep("console.log(123)");
    let matches = grep.root().find_all(&rule.matcher).collect();
    printer.before_print().unwrap();
    let file = SimpleFile::new(Cow::Borrowed("test.ts"), "console.log(123)");
    let buffer = printer
      .get_processor()
      .print_rule(matches, file, &rule)
      .unwrap();
    printer.process(buffer).unwrap();
    printer.after_print().unwrap();
    let json_str = get_text(&printer);
    let json: Vec<RuleMatchJSON> = serde_json::from_str(&json_str).unwrap();
    let labels = &json[0].labels;
    assert_eq!(labels[0].message.unwrap(), "var label");
    assert_eq!(labels[0].style, LabelStyle::Primary);
    assert_eq!(labels[0].text, "123");
  }

  const META_TEXT: &str = "
metadata:
  A: test-meta
";
  #[test]
  fn test_metadata() {
    for included in [true, false] {
      let mut printer = make_test_printer(JsonStyle::Compact).include_metadata(included);
      let rule = get_rule_config(&format!("pattern: console.log($A)\n{META_TEXT}"));
      let grep = SgLang::from(SupportLang::TypeScript).ast_grep("console.log(123)");
      let matches = grep.root().find_all(&rule.matcher).collect();
      printer.before_print().unwrap();
      let file = SimpleFile::new(Cow::Borrowed("test.ts"), "console.log(123)");
      let buffer = printer
        .get_processor()
        .print_rule(matches, file, &rule)
        .unwrap();
      printer.process(buffer).unwrap();
      printer.after_print().unwrap();
      let json_str = get_text(&printer);
      let json: Vec<RuleMatchJSON> = serde_json::from_str(&json_str).unwrap();
      let metadata = &json[0].metadata;
      assert_eq!(metadata.is_some(), included);
    }
  }
}
