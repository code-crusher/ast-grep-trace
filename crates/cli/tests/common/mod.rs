use anyhow::Result;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

pub fn create_test_files<'a>(
  names_and_contents: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<TempDir> {
  let dir = TempDir::new()?;
  for (name, contents) in names_and_contents {
    if let Some(sub) = Path::new(name).parent() {
      let sub_dir = dir.path().join(sub);
      std::fs::create_dir_all(sub_dir)?;
    }
    let path = dir.path().join(name);
    let mut file = File::create(path.clone())?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
  }
  Ok(dir)
}
