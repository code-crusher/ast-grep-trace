//! Load native matcher plugins configured in `sgconfig.yml`.
//!
//! A native plugin is a shared object exporting a function with this C signature:
//!
//! ```c
//! int32_t ast_grep_plugin_match(const uint8_t *input, size_t input_len);
//! ```
//!
//! `input` is a UTF-8 JSON object describing the candidate node, with the
//! shape of [`PluginNode`]. The function returns `1` when the node matches and
//! `0` when it does not. Any other value is treated as no match.
//!
//! ```yaml
//! plugins:
//!   react-components:
//!     libraryPath: ./plugins/libreact_components.so
//!     pluginSymbol: ast_grep_plugin_match  # optional
//! ```
//!
//! Plugins execute arbitrary native code, so the CLI only loads them after the
//! same opt-in as custom languages, `--custom-languages allow`.

use crate::LibraryPath;
use ast_grep_config::{Plugin, PluginNode, PluginRegistry};
use libloading::{Error as LibError, Library, Symbol};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use std::collections::HashMap;
use std::fs::canonicalize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The symbol a native plugin must export unless `pluginSymbol` overrides it.
pub const DEFAULT_PLUGIN_SYMBOL: &str = "ast_grep_plugin_match";

/// Signature of a native plugin matcher.
///
/// # Safety
///
/// `input` must point to `input_len` initialized bytes that stay valid for the
/// duration of the call. Implementations must not unwind across the FFI
/// boundary.
pub type PluginMatchFn = unsafe extern "C" fn(input: *const u8, input_len: usize) -> i32;

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CustomPlugin {
  /// Path to the plugin shared object, or a map of target triples to paths.
  pub library_path: LibraryPath,
  /// The exported function to call. Defaults to [`DEFAULT_PLUGIN_SYMBOL`].
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub plugin_symbol: Option<String>,
}

#[derive(Debug, Error)]
pub enum PluginError {
  #[error("Cannot load plugin `{name}` from `{path}`: {source}")]
  Load {
    name: String,
    path: PathBuf,
    #[source]
    source: LibError,
  },
  #[error("Plugin `{name}` does not export symbol `{symbol}`: {source}")]
  Symbol {
    name: String,
    symbol: String,
    #[source]
    source: LibError,
  },
  #[error("Plugin `{name}` has no library for the current platform `{target}`")]
  NotConfigured { name: String, target: &'static str },
  #[error("Cannot resolve the absolute path of plugin `{name}`: {source}")]
  Path {
    name: String,
    #[source]
    source: std::io::Error,
  },
}

struct NativePlugin {
  // The symbol is copied out as a plain function pointer, but the library must
  // stay loaded for that pointer to remain valid.
  _lib: Library,
  func: PluginMatchFn,
}

impl Plugin for NativePlugin {
  fn is_match(&self, node: &PluginNode<'_>) -> bool {
    let Ok(input) = serde_json::to_vec(node) else {
      return false;
    };
    // SAFETY: `input` is valid for `input.len()` bytes for the whole call.
    // The plugin ABI returns 1 on a match; anything else is a non-match.
    unsafe { (self.func)(input.as_ptr(), input.len()) == 1 }
  }
}

impl CustomPlugin {
  fn library_path(&self, name: &str, base: &Path) -> Result<PathBuf, PluginError> {
    let path = match &self.library_path {
      LibraryPath::Single(path) => path.clone(),
      LibraryPath::Platform(map) => {
        let target = target_tuple::TARGET;
        map
          .get(target)
          .cloned()
          .ok_or_else(|| PluginError::NotConfigured {
            name: name.to_string(),
            target,
          })?
      }
    };
    Ok(base.join(path))
  }

  fn load(self, name: &str, base: &Path) -> Result<NativePlugin, PluginError> {
    let path = self.library_path(name, base)?;
    let symbol = self
      .plugin_symbol
      .unwrap_or_else(|| DEFAULT_PLUGIN_SYMBOL.to_string());
    let abs_path = canonicalize(&path).map_err(|source| PluginError::Path {
      name: name.to_string(),
      source,
    })?;
    // SAFETY: loading a shared object runs its initialization code, which is
    // why plugins require explicit user opt-in.
    unsafe {
      let lib = Library::new(abs_path.as_os_str()).map_err(|source| PluginError::Load {
        name: name.to_string(),
        path,
        source,
      })?;
      let func: Symbol<PluginMatchFn> =
        lib
          .get(symbol.as_bytes())
          .map_err(|source| PluginError::Symbol {
            name: name.to_string(),
            symbol,
            source,
          })?;
      Ok(NativePlugin {
        func: *func,
        _lib: lib,
      })
    }
  }

  /// Load every configured plugin into a registry keyed by plugin name.
  pub fn register(
    base: &Path,
    plugins: HashMap<String, CustomPlugin>,
  ) -> Result<PluginRegistry, PluginError> {
    let mut registry = PluginRegistry::new();
    for (name, plugin) in plugins {
      let loaded = plugin.load(&name, base)?;
      registry.register(name, Arc::new(loaded));
    }
    Ok(registry)
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use ast_grep_config::PluginNode;
  use std::process::Command;

  const PLUGIN_SOURCE: &str = r#"
#[unsafe(no_mangle)]
pub extern "C" fn ast_grep_plugin_match(input: *const u8, input_len: usize) -> i32 {
  // A match when the node text contains "foo".
  let bytes = unsafe { std::slice::from_raw_parts(input, input_len) };
  let json = String::from_utf8_lossy(bytes);
  i32::from(json.contains("\"text\":\"foo"))
}
"#;

  fn dylib_name() -> &'static str {
    if cfg!(target_os = "windows") {
      "test_plugin.dll"
    } else if cfg!(target_os = "macos") {
      "libtest_plugin.dylib"
    } else {
      "libtest_plugin.so"
    }
  }

  /// Compile a tiny plugin shared object with the toolchain that built the test.
  fn build_plugin(dir: &Path) -> Option<PathBuf> {
    let source = dir.join("test_plugin.rs");
    std::fs::write(&source, PLUGIN_SOURCE).ok()?;
    let output = dir.join(dylib_name());
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let status = Command::new(rustc)
      .args(["--edition", "2024", "--crate-type", "cdylib", "-o"])
      .arg(&output)
      .arg(&source)
      .status()
      .ok()?;
    status.success().then_some(output)
  }

  fn node<'a>(kind: &'a str, text: &'a str) -> PluginNode<'a> {
    PluginNode {
      kind,
      text,
      range: 0..text.len(),
      lines: 0..0,
    }
  }

  #[test]
  fn test_load_and_match_native_plugin() {
    let Ok(dir) = tempfile::tempdir() else {
      return;
    };
    let Some(path) = build_plugin(dir.path()) else {
      // No usable toolchain: skip rather than fail on unsupported platforms.
      return;
    };
    let custom = CustomPlugin {
      library_path: LibraryPath::Single(path),
      plugin_symbol: None,
    };
    let mut plugins = HashMap::new();
    plugins.insert("test".to_string(), custom);
    let registry = CustomPlugin::register(dir.path(), plugins).expect("plugin should load");
    let plugin = registry.get("test").expect("plugin should be registered");
    assert!(plugin.is_match(&node("identifier", "foo")));
    assert!(!plugin.is_match(&node("identifier", "bar")));
  }

  #[test]
  fn test_missing_symbol_reports_error() {
    let Ok(dir) = tempfile::tempdir() else {
      return;
    };
    let Some(path) = build_plugin(dir.path()) else {
      return;
    };
    let custom = CustomPlugin {
      library_path: LibraryPath::Single(path),
      plugin_symbol: Some("missing_symbol".to_string()),
    };
    let mut plugins = HashMap::new();
    plugins.insert("test".to_string(), custom);
    let err = match CustomPlugin::register(dir.path(), plugins) {
      Err(e) => e,
      Ok(_) => panic!("symbol should be missing"),
    };
    assert!(matches!(err, PluginError::Symbol { name, .. } if name == "test"));
  }
}
