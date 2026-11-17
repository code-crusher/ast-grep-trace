//! A minimal in-terminal pager for interactive output.
//!
//! Interactive mode prints matches inside the alternate screen. A single
//! screen can only show a limited number of lines, so a match with a lot of
//! context (or a file with many matches) overflows and the top part becomes
//! unreadable. This module wraps the rendered output into terminal rows and
//! lets the user scroll through them, much like `less` does.

use anyhow::Result;
use crossterm::{
  cursor::{self, MoveTo},
  event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
  execute,
  terminal::{self, Clear, ClearType, DisableLineWrap, EnableLineWrap},
};
use std::io::{stdout, Write};
use std::ops::Range;
use unicode_width::UnicodeWidthChar;

const FALLBACK_SIZE: (u16, u16) = (80, 24);
const ANSI_RESET: &str = "\x1b[0m";
/// Columns between tab stops in most terminals.
const TAB_WIDTH: usize = 8;

/// Display `content` in a scrollable view and read the user's reply to `prompt_text`.
///
/// This mirrors [`super::prompt`]: `letters` are the accepted answers and `default`
/// is returned when the user presses enter. Scroll keys (`↑`/`↓`, `PgUp`/`PgDn`,
/// `j`/`k`, `d`/`u`, `g`/`G`) are handled by the pager itself and never reach the
/// caller, so the same answers keep working as before.
pub fn prompt_with_pager(
  content: &str,
  prompt_text: &str,
  letters: &str,
  default: Option<char>,
) -> Result<char> {
  let mut out = stdout();
  terminal::enable_raw_mode()?;
  let result = run_pager(&mut out, content, prompt_text, letters, default);
  // Restore the terminal even when the pager fails to read a key.
  let _ = execute!(out, EnableLineWrap, cursor::Show);
  terminal::disable_raw_mode()?;
  result
}

fn run_pager(
  out: &mut impl Write,
  content: &str,
  prompt_text: &str,
  letters: &str,
  default: Option<char>,
) -> Result<char> {
  let (width, mut height) = terminal::size().unwrap_or(FALLBACK_SIZE);
  let mut rows = wrap_ansi_lines(content, usize::from(width.max(1)));
  let mut view = PageView::new(rows.len(), content_height(height));
  execute!(out, DisableLineWrap, cursor::Hide)?;
  let mut dirty = true;
  loop {
    if dirty {
      draw_page(out, &rows, &view, prompt_text)?;
      dirty = false;
    }
    match event::read()? {
      Event::Key(key) if key.kind != KeyEventKind::Release => {
        match classify_key(key, letters, default, view.height) {
          Some(PagerReply::Answer(answer)) => return Ok(answer),
          Some(PagerReply::Scroll(delta)) => {
            let old_offset = view.offset;
            view.scroll(delta);
            dirty = view.offset != old_offset;
          }
          Some(PagerReply::Top) => {
            dirty = view.offset != 0;
            view.to_top();
          }
          Some(PagerReply::Bottom) => {
            dirty = view.offset != view.max_offset();
            view.to_bottom();
          }
          None => {}
        }
      }
      Event::Resize(new_width, new_height) => {
        height = new_height;
        let offset = view.offset;
        rows = wrap_ansi_lines(content, usize::from(new_width.max(1)));
        view = PageView::new(rows.len(), content_height(height));
        view.scroll_to(offset);
        dirty = true;
      }
      _ => {}
    }
  }
}

fn content_height(height: u16) -> usize {
  usize::from(height.saturating_sub(1).max(1))
}

fn draw_page(
  out: &mut impl Write,
  rows: &[String],
  view: &PageView,
  prompt_text: &str,
) -> Result<()> {
  let visible = view.visible();
  execute!(out, Clear(ClearType::All), MoveTo(0, 0))?;
  for (i, row) in rows[visible.clone()].iter().enumerate() {
    if i > 0 {
      // Do not emit a trailing newline: it would scroll the last row away.
      write!(out, "\r\n")?;
    }
    write!(out, "{row}")?;
  }
  // Make sure an unterminated style cannot leak into the prompt.
  write!(out, "{ANSI_RESET}")?;
  let prompt = if view.max_offset() > 0 {
    format!(
      "{prompt_text} Scroll[PgUp/PgDn] {}-{}/{}",
      visible.start + 1,
      visible.end,
      view.total
    )
  } else {
    prompt_text.to_string()
  };
  execute!(out, MoveTo(0, visible.len() as u16))?;
  write!(out, "{prompt}")?;
  out.flush()?;
  Ok(())
}

enum PagerReply {
  Answer(char),
  Scroll(isize),
  Top,
  Bottom,
}

fn classify_key(
  key: KeyEvent,
  letters: &str,
  default: Option<char>,
  page: usize,
) -> Option<PagerReply> {
  use PagerReply as R;
  let page = page.max(1);
  if key.modifiers.contains(KeyModifiers::CONTROL) {
    return match key.code {
      // Ctrl+C keeps its old meaning of answering "quit" where accepted.
      KeyCode::Char('c') if letters.contains('q') => Some(R::Answer('q')),
      KeyCode::Char('d') => Some(R::Scroll(page as isize)),
      KeyCode::Char('u') => Some(R::Scroll(-(page as isize))),
      _ => None,
    };
  }
  match key.code {
    KeyCode::Enter => Some(R::Answer(default.unwrap_or('\n'))),
    KeyCode::Tab if letters.contains('\t') => Some(R::Answer('\t')),
    KeyCode::Char(c) if letters.contains(c) => Some(R::Answer(c)),
    KeyCode::Char('j') | KeyCode::Down => Some(R::Scroll(1)),
    KeyCode::Char('k') | KeyCode::Up => Some(R::Scroll(-1)),
    KeyCode::Char(' ') | KeyCode::PageDown => Some(R::Scroll(page as isize)),
    KeyCode::PageUp => Some(R::Scroll(-(page as isize))),
    KeyCode::Char('d') => Some(R::Scroll((page / 2).max(1) as isize)),
    KeyCode::Char('u') => Some(R::Scroll(-((page / 2).max(1) as isize))),
    KeyCode::Char('g') | KeyCode::Home => Some(R::Top),
    KeyCode::Char('G') | KeyCode::End => Some(R::Bottom),
    _ => None,
  }
}

/// The scroll position of a page of `height` rows out of `total` rows.
struct PageView {
  offset: usize,
  total: usize,
  height: usize,
}

impl PageView {
  fn new(total: usize, height: usize) -> Self {
    Self {
      offset: 0,
      total,
      height: height.max(1),
    }
  }

  fn max_offset(&self) -> usize {
    self.total.saturating_sub(self.height)
  }

  fn visible(&self) -> Range<usize> {
    let end = self.offset.saturating_add(self.height).min(self.total);
    self.offset..end
  }

  fn scroll(&mut self, delta: isize) {
    let target = if delta >= 0 {
      self.offset.saturating_add(delta.unsigned_abs())
    } else {
      self.offset.saturating_sub(delta.unsigned_abs())
    };
    self.offset = target.min(self.max_offset());
  }

  fn scroll_to(&mut self, offset: usize) {
    self.offset = offset.min(self.max_offset());
  }

  fn to_top(&mut self) {
    self.offset = 0;
  }

  fn to_bottom(&mut self) {
    self.offset = self.max_offset();
  }
}

/// Split ANSI-colored `text` into terminal rows no wider than `width` columns.
///
/// Escape sequences are preserved and do not count towards the width. Styles
/// that are still active when a row wraps are re-applied on the next row.
fn wrap_ansi_lines(text: &str, width: usize) -> Vec<String> {
  let width = width.max(1);
  let mut rows = Vec::new();
  for line in text.lines() {
    wrap_ansi_line(line, width, &mut rows);
  }
  rows
}

fn wrap_ansi_line(line: &str, width: usize, rows: &mut Vec<String>) {
  let bytes = line.as_bytes();
  let mut row = String::new();
  // SGR sequences seen so far, replayed at the start of a new row so the
  // active style survives the wrap. Resets remove them again.
  let mut prefix = String::new();
  let mut col = 0;
  let mut idx = 0;
  while idx < bytes.len() {
    if let Some(len) = escape_seq_len(bytes, idx) {
      let escape = &line[idx..idx + len];
      if is_sgr_reset(escape) {
        prefix.clear();
      } else if is_sgr(escape) {
        prefix.push_str(escape);
      }
      row.push_str(escape);
      idx += len;
      continue;
    }
    let ch = line[idx..]
      .chars()
      .next()
      .expect("index should be a char boundary");
    let char_width = display_width(ch, col);
    if col > 0 && col + char_width > width {
      if !prefix.is_empty() {
        row.push_str(ANSI_RESET);
      }
      rows.push(std::mem::take(&mut row));
      row.push_str(&prefix);
      col = 0;
      // Re-evaluate the character's width at the start of the new row.
      continue;
    }
    if ch == '\t' {
      // Expand tabs so wrapping keeps the original column alignment.
      row.extend(std::iter::repeat_n(' ', char_width));
    } else {
      row.push(ch);
    }
    col += char_width;
    idx += ch.len_utf8();
  }
  rows.push(row);
}

fn display_width(ch: char, col: usize) -> usize {
  if ch == '\t' {
    TAB_WIDTH - col % TAB_WIDTH
  } else {
    UnicodeWidthChar::width(ch).unwrap_or(0)
  }
}

/// Return the byte length of the escape sequence starting at `start`, if any.
fn escape_seq_len(bytes: &[u8], start: usize) -> Option<usize> {
  if bytes.get(start) != Some(&0x1b) {
    return None;
  }
  match bytes.get(start + 1)? {
    // Control Sequence Introducer, terminated by a byte in 0x40..=0x7e.
    b'[' => {
      let mut i = start + 2;
      while i < bytes.len() {
        let byte = bytes[i];
        i += 1;
        if (0x40..=0x7e).contains(&byte) {
          return Some(i - start);
        }
      }
      Some(bytes.len() - start)
    }
    // Operating System Command, terminated by BEL or ST.
    b']' => {
      let mut i = start + 2;
      while i < bytes.len() {
        if bytes[i] == 0x07 {
          return Some(i + 1 - start);
        }
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
          return Some(i + 2 - start);
        }
        i += 1;
      }
      Some(bytes.len() - start)
    }
    _ => Some(2.min(bytes.len() - start)),
  }
}

/// Whether `escape` is a Select Graphic Rendition sequence, i.e. sets styles.
fn is_sgr(escape: &str) -> bool {
  escape.starts_with("\x1b[") && escape.ends_with('m')
}

/// Whether `escape` is a Select Graphic Rendition sequence resetting all styles.
fn is_sgr_reset(escape: &str) -> bool {
  is_sgr(escape) && {
    let params = &escape[2..escape.len() - 1];
    params
      .split(';')
      .all(|param| param.is_empty() || param == "0")
  }
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn wraps_plain_text() {
    let rows = wrap_ansi_lines("abcdefg", 3);
    assert_eq!(rows, ["abc", "def", "g"]);
  }

  #[test]
  fn keeps_short_lines_intact() {
    let rows = wrap_ansi_lines("a\nbb\n", 10);
    assert_eq!(rows, ["a", "bb"]);
  }

  #[test]
  fn ignores_escape_sequences_in_width() {
    let colored = format!("{ANSI_RESET}abcdef");
    let rows = wrap_ansi_lines(&colored, 3);
    // A reset carries no style over to the next row.
    assert_eq!(rows, [format!("{ANSI_RESET}abc"), "def".to_string()]);
  }

  #[test]
  fn re_applies_active_style_on_wrapped_rows() {
    let colored = format!("\x1b[31mabcdef\x1b[0m");
    let rows = wrap_ansi_lines(&colored, 3);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], format!("\x1b[31mabc{ANSI_RESET}"));
    assert_eq!(rows[1], format!("\x1b[31mdef\x1b[0m"));
  }

  #[test]
  fn wraps_wide_characters_by_display_width() {
    let rows = wrap_ansi_lines("你好", 3);
    assert_eq!(rows, ["你", "好"]);
  }

  #[test]
  fn tab_occupies_next_tab_stop() {
    let rows = wrap_ansi_lines("a\tb", 8);
    assert_eq!(rows, ["a       ", "b"]);
  }

  #[test]
  fn non_sgr_escape_is_not_carried_over() {
    // A cursor-movement sequence is not a style and must not be replayed.
    let text = "\x1b[2Kabc";
    let rows = wrap_ansi_lines(text, 2);
    assert_eq!(rows, ["\x1b[2Kab", "c"]);
  }

  fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
  }

  fn reply(key: KeyEvent, letters: &str, default: Option<char>, page: usize) -> Option<char> {
    match classify_key(key, letters, default, page) {
      Some(PagerReply::Answer(answer)) => Some(answer),
      _ => None,
    }
  }

  #[test]
  fn classify_key_keeps_prompt_answers() {
    assert_eq!(reply(press(KeyCode::Char('y')), "ynaqe\t", Some('y'), 10), Some('y'));
    assert_eq!(reply(press(KeyCode::Char('q')), "ynaqe\t", Some('y'), 10), Some('q'));
    assert_eq!(reply(press(KeyCode::Enter), "qe", Some('\n'), 10), Some('\n'));
    assert_eq!(reply(press(KeyCode::Tab), "ynaqe\t", Some('y'), 10), Some('\t'));
  }

  #[test]
  fn classify_key_rejects_letters_outside_the_prompt() {
    // 'j' is a scroll key and must not answer a prompt that does not accept it.
    assert_eq!(reply(press(KeyCode::Char('j')), "ynaqe\t", Some('y'), 10), None);
    assert_eq!(reply(press(KeyCode::Char('x')), "ynaqe\t", Some('y'), 10), None);
  }

  #[test]
  fn classify_key_scrolls() {
    let page = 10;
    let scroll = |key| match classify_key(key, "q", None, page) {
      Some(PagerReply::Scroll(delta)) => Some(delta),
      _ => None,
    };
    assert_eq!(scroll(press(KeyCode::Char('j'))), Some(1));
    assert_eq!(scroll(press(KeyCode::Down)), Some(1));
    assert_eq!(scroll(press(KeyCode::Char('k'))), Some(-1));
    assert_eq!(scroll(press(KeyCode::PageDown)), Some(10));
    assert_eq!(scroll(press(KeyCode::PageUp)), Some(-10));
    assert_eq!(scroll(press(KeyCode::Char(' '))), Some(10));
  }

  #[test]
  fn classify_key_handles_ctrl_c_only_for_quit_prompts() {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(reply(ctrl_c, "qe", Some('\n'), 10), Some('q'));
    assert_eq!(reply(ctrl_c, "ynaqe\t", Some('y'), 10), Some('q'));
    // A prompt without 'q' must not turn Ctrl+C into a bogus answer.
    assert_eq!(reply(ctrl_c, "e", Some('\n'), 10), None);
  }

  #[test]
  fn page_view_scrolls_within_bounds() {
    let mut view = PageView::new(100, 10);
    assert_eq!(view.visible(), 0..10);
    view.scroll(5);
    assert_eq!(view.visible(), 5..15);
    view.scroll(-100);
    assert_eq!(view.visible(), 0..10);
    view.scroll(1000);
    assert_eq!(view.offset, 90);
    assert_eq!(view.visible(), 90..100);
  }

  #[test]
  fn page_view_handles_short_content() {
    let mut view = PageView::new(3, 10);
    assert_eq!(view.max_offset(), 0);
    view.scroll(5);
    assert_eq!(view.visible(), 0..3);
    view.to_bottom();
    assert_eq!(view.offset, 0);
  }

  #[test]
  fn page_view_restores_offset_on_resize() {
    let mut view = PageView::new(100, 10);
    view.scroll(50);
    view = PageView::new(100, 40);
    view.scroll_to(50);
    assert_eq!(view.visible(), 50..90);
    view = PageView::new(100, 10);
    view.scroll_to(90);
    assert_eq!(view.visible(), 90..100);
  }
}
