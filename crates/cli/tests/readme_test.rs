//! Guards the "Who is using ast-grep" section in the root README.
//! The section is fed by user comments on issue #373, so it is easy for a
//! hand-edited row to break the table or lose its link.

const README: &str = include_str!("../../../README.md");

fn section() -> &'static str {
  let start = README
    .find("## Who is using ast-grep")
    .expect("README should have a 'Who is using ast-grep' section");
  let rest = &README[start..];
  let end = rest.find("\n## ").unwrap_or(rest.len());
  &rest[..end]
}

#[test]
fn who_is_using_section_exists() {
  let section = section();
  assert!(
    section.contains("https://github.com/ast-grep/ast-grep/issues/373"),
    "section should link to issue #373 so new users can sign up"
  );
}

#[test]
fn who_is_using_table_is_well_formed() {
  let rows: Vec<&str> = section()
    .lines()
    .filter(|line| line.starts_with('|'))
    .collect();
  assert!(
    rows.len() > 2,
    "section should have a header and at least one user"
  );
  let columns = rows[0].matches('|').count();
  for row in &rows {
    assert_eq!(
      row.matches('|').count(),
      columns,
      "every table row should have the same number of columns: {row}"
    );
  }
}

#[test]
fn who_is_using_entries_have_links_and_logos() {
  let rows = section().lines().filter(|line| line.starts_with('|'));
  // skip header and the `| --- |` separator
  for row in rows.skip(2) {
    let cells: Vec<&str> = row.trim_matches('|').split('|').map(str::trim).collect();
    assert_eq!(
      cells.len(),
      3,
      "each user row should be name, website and logo: {row}"
    );
    let (name, website, logo) = (cells[0], cells[1], cells[2]);
    assert!(!name.is_empty(), "user row should have a name: {row}");
    assert!(
      website.starts_with("https://"),
      "user {name} should link to an https website: {row}"
    );
    assert!(
      logo.contains("<img ") && logo.contains("src=\"https://"),
      "user {name} should have an https logo image: {row}"
    );
  }
}
