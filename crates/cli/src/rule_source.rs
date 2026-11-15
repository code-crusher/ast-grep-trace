use std::{
  fs,
  io::Read,
  path::{Path, PathBuf},
  time::Duration,
};

use anyhow::{Context, Result};

const GITHUB_HOST: &str = "github.com";
/// Bump to invalidate on-disk caches written by older ast-grep versions.
const CACHE_REVISION: u32 = 1;
const STAMP_FILE: &str = ".ast-grep-downloaded";
/// Upper bound for the downloaded tarball; rule directories are tiny.
const MAX_DOWNLOAD_SIZE: u64 = 20 * 1024 * 1024;
const HTTP_USER_AGENT: &str = concat!("ast-grep/", env!("CARGO_PKG_VERSION"));

/// A `ruleDirs`/`utilDirs` entry: either a directory relative to the project
/// or a remote directory referenced by a GitHub URL like
/// `https://github.com/user/repo/tree/main/rules`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSource {
  Local(PathBuf),
  Remote(GitHubDir),
}

impl RuleSource {
  /// Interpret one raw `ruleDirs`/`utilDirs` entry. Entries under a URL
  /// scheme pointing at github.com are resolved as remote sources; everything
  /// else is kept as a project-relative path, as before.
  pub fn from_entry(entry: &Path) -> Result<Self> {
    match GitHubDir::parse(entry) {
      Some(remote) => Ok(Self::Remote(remote)),
      None => Ok(Self::Local(entry.to_path_buf())),
    }
  }
}

/// A rule directory living in a GitHub repository, e.g. parsed from
/// `https://github.com/user/repo/tree/<ref>/<path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubDir {
  owner: String,
  repo: String,
  /// A branch, tag or commit; `HEAD` refers to the repository's default branch.
  git_ref: String,
  path: String,
}

impl GitHubDir {
  /// Recognize `https?://github.com/<owner>/<repo>[/tree/<ref>][/<path>]`.
  /// A bare repository URL refers to its root directory.
  fn parse(entry: &Path) -> Option<Self> {
    let s = entry.to_str()?;
    let rest = s
      .strip_prefix("https://")
      .or_else(|| s.strip_prefix("http://"))?;
    let rest = rest.strip_prefix(GITHUB_HOST)?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let mut parts = rest.split('/').filter(|p| !p.is_empty());
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    let mut git_ref = "HEAD".to_string();
    let mut path = String::new();
    match parts.next() {
      Some("tree") => {
        git_ref = parts.next()?.to_string();
        path = parts.collect::<Vec<_>>().join("/");
      }
      // a repo subdirectory without an explicit ref
      Some(sub_dir) => {
        path = Some(sub_dir)
          .into_iter()
          .chain(parts)
          .collect::<Vec<_>>()
          .join("/")
      }
      None => {}
    }
    Some(Self {
      owner,
      repo,
      git_ref,
      path,
    })
  }

  /// Download the rule directory into the ast-grep cache and return the local
  /// path. A previous download is reused if it is still on disk.
  pub fn download(&self) -> Result<PathBuf> {
    let dir = self.cache_dir();
    if dir.join(STAMP_FILE).exists() {
      return Ok(dir);
    }
    let tarball = http_get_bytes(&self.codeload_url())
      .with_context(|| format!("Failed to download rules from {self}"))?;
    extract_dir_from_tarball(&tarball, &self.path, &dir)
      .with_context(|| format!("Failed to extract rules from {self}"))?;
    fs::write(dir.join(STAMP_FILE), "")?;
    Ok(dir)
  }

  /// Download the whole repository as a gzipped tarball. Unlike the contents
  /// API, codeload is not rate limited and works for any branch, tag, or
  /// commit SHA.
  fn codeload_url(&self) -> String {
    format!(
      "https://codeload.github.com/{}/{}/tar.gz/{}",
      self.owner, self.repo, self.git_ref
    )
  }

  fn cache_dir(&self) -> PathBuf {
    // Prefer the user-level cache directory; fall back to the system temp
    // directory when it is unavailable (e.g. no $HOME).
    let base = directories::ProjectDirs::from("", "", "ast-grep")
      .map(|dirs| dirs.cache_dir().to_path_buf())
      .unwrap_or_else(std::env::temp_dir);
    base.join(format!(
      "rule-cache/v{CACHE_REVISION}/{}/{}/{}/{}",
      url_safe(&self.owner),
      url_safe(&self.repo),
      url_safe(&self.git_ref),
      url_safe(&self.path),
    ))
  }
}

impl std::fmt::Display for GitHubDir {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "https://github.com/{}/{}/tree/{}/{}",
      self.owner, self.repo, self.git_ref, self.path
    )
  }
}

/// Extract `<tarball-root>/<dir>` from a GitHub repository tarball into
/// `dest`, keeping only files. An empty `dir` extracts the repository root.
fn extract_dir_from_tarball(tarball: &[u8], dir: &str, dest: &Path) -> Result<()> {
  let cursor = std::io::Cursor::new(tarball);
  let gz_decoder = flate2::read::GzDecoder::new(cursor);
  let mut archive = tar::Archive::new(gz_decoder);
  let mut entries = archive.entries()?;
  // GitHub tarballs are rooted in a single `<repo>-<ref>/` directory, but they
  // may start with metadata entries like `pax_global_header`, so find the
  // first directory entry instead of assuming the first entry is the root.
  let root = loop {
    let Some(entry) = entries.next().transpose()? else {
      anyhow::bail!("tarball contains no directories");
    };
    if !entry.header().entry_type().is_dir() {
      continue;
    }
    let entry_path = entry.path()?.to_path_buf();
    break entry_path
      .iter()
      .next()
      .map(|c| c.to_string_lossy().into_owned())
      .ok_or_else(|| anyhow::anyhow!("tarball entry has no root directory"))?;
  };
  let prefix = Path::new(&root).join(dir);
  let mut extracted = 0;
  // process the remaining entries; the first directory entry itself is skipped
  for entry in entries {
    let mut entry = entry?;
    if !entry.header().entry_type().is_file() {
      continue;
    }
    let path = entry.path()?.to_path_buf();
    let Some(relative) = path
      .strip_prefix(&prefix)
      .ok()
      .filter(|p| !p.as_os_str().is_empty())
    else {
      // a rule dir must not leak files from sibling directories sharing the
      // prefix, e.g. `rules` must not match `rules-extra/x.yml`
      continue;
    };
    let output = dest.join(relative);
    if let Some(parent) = output.parent() {
      fs::create_dir_all(parent)?;
    }
    entry.unpack(&output)?;
    extracted += 1;
  }
  if extracted == 0 {
    anyhow::bail!("no rule files found under `{dir}`");
  }
  Ok(())
}

fn http_get_bytes(url: &str) -> Result<Vec<u8>> {
  let agent = ureq::AgentBuilder::new()
    .timeout_connect(Duration::from_secs(10))
    .timeout(Duration::from_secs(60))
    .build();
  let response = agent
    .get(url)
    .set("User-Agent", HTTP_USER_AGENT)
    .call()
    .map_err(Box::new)?;
  let mut bytes = vec![];
  response
    .into_reader()
    .take(MAX_DOWNLOAD_SIZE)
    .read_to_end(&mut bytes)
    .context("Failed to read response body")?;
  Ok(bytes)
}

/// Encode a string so it is safe as a single path segment of the cache dir.
fn url_safe(s: &str) -> String {
  s.bytes()
    .map(|b| {
      if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
        char::from(b).to_string()
      } else {
        format!("%{b:02X}")
      }
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn remote(entry: &str) -> GitHubDir {
    match RuleSource::from_entry(Path::new(entry)).unwrap() {
      RuleSource::Remote(remote) => remote,
      _ => panic!("entry should be remote: {entry}"),
    }
  }

  fn local(entry: &str) -> PathBuf {
    match RuleSource::from_entry(Path::new(entry)).unwrap() {
      RuleSource::Local(path) => path,
      _ => panic!("entry should be local: {entry}"),
    }
  }

  #[test]
  fn parse_rule_sources() {
    assert_eq!(local("./rules"), Path::new("./rules"));
    assert_eq!(local("rules"), Path::new("rules"));
    // non-URL entries with a github.com substring stay local
    assert_eq!(
      local("github.com/user/repo"),
      Path::new("github.com/user/repo")
    );
    // other hosts are not remote sources
    assert_eq!(
      local("https://gitlab.com/user/repo"),
      Path::new("https://gitlab.com/user/repo")
    );
    assert_eq!(
      remote("https://github.com/user/repo/tree/main/rules"),
      GitHubDir {
        owner: "user".into(),
        repo: "repo".into(),
        git_ref: "main".into(),
        path: "rules".into()
      }
    );
    // nested paths and trailing slash
    assert_eq!(
      remote("https://github.com/o/r/tree/v1.2.3/a/b/"),
      GitHubDir {
        owner: "o".into(),
        repo: "r".into(),
        git_ref: "v1.2.3".into(),
        path: "a/b".into()
      }
    );
    // bare repository URL refers to the repo root
    assert_eq!(
      remote("https://github.com/user/repo"),
      GitHubDir {
        owner: "user".into(),
        repo: "repo".into(),
        git_ref: "HEAD".into(),
        path: String::new()
      }
    );
    // repo subdirectory without a ref: ambiguous with the `<repo>/tree/<ref>`
    // shape is not possible (`tree` is a reserved name), so `dir` is a path
    assert_eq!(
      remote("https://github.com/user/repo/dir"),
      GitHubDir {
        owner: "user".into(),
        repo: "repo".into(),
        git_ref: "HEAD".into(),
        path: "dir".into()
      }
    );
    // too few segments to identify a repository
    assert_eq!(
      local("https://github.com/user"),
      Path::new("https://github.com/user")
    );
  }

  #[test]
  fn build_request_urls() {
    let rules_dir = remote("https://github.com/user/repo/tree/main/rules");
    assert_eq!(
      rules_dir.codeload_url(),
      "https://codeload.github.com/user/repo/tar.gz/main"
    );
    let root_dir = remote("https://github.com/user/repo");
    assert_eq!(
      root_dir.codeload_url(),
      "https://codeload.github.com/user/repo/tar.gz/HEAD"
    );
  }

  #[test]
  fn cache_dir_is_unique_per_source() {
    let a = remote("https://github.com/user/repo/tree/main/rules");
    let b = remote("https://github.com/user/repo/tree/dev/rules");
    let c = remote("https://github.com/user/repo/tree/main/other");
    assert_ne!(a.cache_dir(), b.cache_dir());
    assert_ne!(a.cache_dir(), c.cache_dir());
    assert!(a.cache_dir().to_string_lossy().contains("rule-cache"));
  }

  #[test]
  fn url_safe_escapes_unusual_characters() {
    assert_eq!(url_safe("user-name_1.2"), "user-name_1.2");
    assert_eq!(url_safe("a/b"), "a%2Fb");
    assert_eq!(url_safe("汉"), "%E6%B1%89");
  }

  #[test]
  fn extract_rules_from_tarball() {
    let tarball = fs::read(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../../fixtures/test-repo.tar.gz"
    ))
    .expect("fixture tarball should be committed");
    let dest = tempfile::TempDir::new().unwrap();
    extract_dir_from_tarball(&tarball, "rules", dest.path()).unwrap();
    let rule = fs::read_to_string(dest.path().join("no-debugger.yml")).unwrap();
    assert!(rule.contains("id: no-debugger"));
    // files outside the requested directory are not extracted
    assert!(!dest.path().join("README.md").exists());
    // nested directories are preserved
    let dest_sub = tempfile::TempDir::new().unwrap();
    extract_dir_from_tarball(&tarball, "docs/sub", dest_sub.path()).unwrap();
    let nested = fs::read_to_string(dest_sub.path().join("nested.yml")).unwrap();
    assert!(nested.contains("id: nested"));
    // a directory with the requested dir as prefix is not confused with it
    let dest2 = tempfile::TempDir::new().unwrap();
    extract_dir_from_tarball(&tarball, "rules-extra", dest2.path()).unwrap();
    let leaked = fs::read_to_string(dest2.path().join("x.yml")).unwrap();
    assert!(leaked.contains("id: in-rules-extra"));
  }

  #[test]
  fn extract_missing_dir_fails() {
    let tarball = fs::read(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../../fixtures/test-repo.tar.gz"
    ))
    .expect("fixture tarball should be committed");
    let dest = tempfile::TempDir::new().unwrap();
    assert!(extract_dir_from_tarball(&tarball, "no-such-dir", dest.path()).is_err());
  }
}
