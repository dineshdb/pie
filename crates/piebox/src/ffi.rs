//! Runtime binding to libkrun.
//!
//! libkrun is `dlopen`'d instead of linked, so piebox compiles and tests on any
//! host (CI included) and fails with a clear message when the library is absent.
//! The alternative, `krun-sys`, needs libkrun headers plus pkg-config at build
//! time, which would make the whole workspace unbuildable without libkrun.

use crate::error::{Error, Result};
use libloading::{Library, Symbol};
use std::path::{Path, PathBuf};

/// Resolved libkrun entry points.
///
/// Only the symbols piebox actually calls are resolved; a missing one is
/// reported by name so an outdated libkrun is obvious.
pub(crate) struct Krun {
    /// Kept alive so the resolved function pointers stay valid.
    _lib: Library,
    pub(crate) set_log_level: unsafe extern "C" fn(u32) -> i32,
    pub(crate) create_ctx: unsafe extern "C" fn() -> i32,
    pub(crate) free_ctx: unsafe extern "C" fn(u32) -> i32,
    pub(crate) set_vm_config: unsafe extern "C" fn(u32, u8, u32) -> i32,
    pub(crate) get_max_vcpus: unsafe extern "C" fn() -> i32,
    pub(crate) has_feature: unsafe extern "C" fn(u64) -> i32,
}

impl Krun {
    /// Loads libkrun from `path`.
    pub(crate) fn load_from(path: &Path) -> Result<Self> {
        // SAFETY: dlopen runs the library's initializers. libkrun is a plain
        // VMM library with no unusual load-time behaviour.
        let lib = unsafe { Library::new(path) }.map_err(|source| Error::LibraryLoad {
            candidates: vec![path.to_path_buf()],
            source: Some(source),
        })?;
        Self::from_library(lib)
    }

    /// Tries each candidate in order, reporting all of them if none load.
    pub(crate) fn load_any(candidates: &[PathBuf]) -> Result<(Self, PathBuf)> {
        let mut last = None;
        for path in candidates {
            // SAFETY: see `load_from`.
            match unsafe { Library::new(path) } {
                Ok(lib) => return Ok((Self::from_library(lib)?, path.clone())),
                Err(err) => last = Some(err),
            }
        }
        Err(Error::LibraryLoad {
            candidates: candidates.to_vec(),
            source: last,
        })
    }

    fn from_library(lib: Library) -> Result<Self> {
        // SAFETY: every signature below is transcribed from libkrun.h and the
        // pointers stay valid because `_lib` keeps the library loaded.
        unsafe {
            Ok(Self {
                set_log_level: sym(&lib, "krun_set_log_level")?,
                create_ctx: sym(&lib, "krun_create_ctx")?,
                free_ctx: sym(&lib, "krun_free_ctx")?,
                set_vm_config: sym(&lib, "krun_set_vm_config")?,
                get_max_vcpus: sym(&lib, "krun_get_max_vcpus")?,
                has_feature: sym(&lib, "krun_has_feature")?,
                _lib: lib,
            })
        }
    }
}

/// Resolves one symbol, copying the pointer out of the borrowed `Symbol`.
///
/// # Safety
/// `T` must match the symbol's real signature.
unsafe fn sym<T: Copy>(lib: &Library, name: &'static str) -> Result<T> {
    let symbol: Symbol<T> =
        unsafe { lib.get(name) }.map_err(|source| Error::Symbol { name, source })?;
    Ok(*symbol)
}

/// Library file names for the host platform, versioned first.
///
/// The versioned name is the real install name (`otool -D` reports
/// `libkrun.1.dylib`); the unversioned one is a development symlink that only
/// exists with the full formula.
const fn library_file_names() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["libkrun.1.dylib", "libkrun.dylib"]
    } else {
        &["libkrun.so.1", "libkrun.so"]
    }
}

/// Candidate libkrun locations, most specific first.
///
/// `PIEBOX_LIBKRUN` wins outright. Otherwise Homebrew's prefix is tried before
/// the bare file names, because dyld's fallback path is `~/lib:/usr/local/lib:
/// /usr/lib` and never includes `/opt/homebrew` — so the explicit directories
/// are load-bearing on macOS, and the bare name only helps Linux's ld.so cache.
pub(crate) fn candidates(
    env_override: Option<&str>,
    homebrew_prefix: Option<&str>,
) -> Vec<PathBuf> {
    if let Some(explicit) = env_override.filter(|s| !s.is_empty()) {
        return vec![PathBuf::from(explicit)];
    }

    let mut out: Vec<PathBuf> = Vec::new();
    // Deduplicate on insert: the list is short and unsorted, so `Vec::dedup`
    // (consecutive only) would miss e.g. HOMEBREW_PREFIX=/usr/local.
    let push = |out: &mut Vec<PathBuf>, path: PathBuf| {
        if !out.contains(&path) {
            out.push(path);
        }
    };

    for file in library_file_names() {
        if let Some(prefix) = homebrew_prefix.filter(|s| !s.is_empty()) {
            push(&mut out, Path::new(prefix).join("lib").join(file));
        }
        for dir in ["/opt/homebrew/lib", "/usr/local/lib", "/usr/lib"] {
            push(&mut out, Path::new(dir).join(file));
        }
        // Last resort: let the dynamic loader search its own paths.
        push(&mut out, PathBuf::from(*file));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_short_circuits() {
        let c = candidates(Some("/tmp/libkrun.dylib"), Some("/opt/homebrew"));
        assert_eq!(c, vec![PathBuf::from("/tmp/libkrun.dylib")]);
    }

    #[test]
    fn empty_env_override_is_ignored() {
        let c = candidates(Some(""), None);
        assert!(c.len() > 1, "{c:?}");
    }

    #[test]
    fn homebrew_prefix_is_tried_first() {
        let c = candidates(None, Some("/custom/brew"));
        let expected = Path::new("/custom/brew/lib").join(library_file_names()[0]);
        assert_eq!(c[0], expected);
    }

    #[test]
    fn candidates_include_bare_file_names_for_loader_search() {
        let c = candidates(None, None);
        for file in library_file_names() {
            assert!(
                c.contains(&PathBuf::from(*file)),
                "{file} missing from {c:?}"
            );
        }
    }

    /// A plain `Vec::dedup` misses these: the duplicate is not adjacent unless
    /// the prefix happens to be `/opt/homebrew`.
    #[test]
    fn duplicates_are_removed_for_any_prefix() {
        for prefix in ["/opt/homebrew", "/usr/local", "/usr"] {
            let c = candidates(None, Some(prefix));
            let mut sorted = c.clone();
            sorted.sort();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(sorted.len(), before, "duplicate for prefix {prefix}: {c:?}");
        }
    }

    #[test]
    fn versioned_library_name_is_tried_before_unversioned() {
        let names = library_file_names();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names[0].contains('1'), "{names:?}");
        assert!(!names[1].contains('1'), "{names:?}");
    }
}
