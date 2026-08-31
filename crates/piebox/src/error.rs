use std::fmt;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// libkrun could not be dlopen'd from any candidate path.
    LibraryLoad {
        candidates: Vec<PathBuf>,
        /// The last dlopen failure; `None` when there was nothing to try.
        source: Option<libloading::Error>,
    },
    /// The library loaded but a required symbol is missing (version too old).
    Symbol {
        name: &'static str,
        source: libloading::Error,
    },
    /// A libkrun call returned a negative errno.
    Krun { call: &'static str, code: i32 },
    /// A value rejected before it ever reached libkrun.
    InvalidValue { what: &'static str, detail: String },
}

impl Error {
    pub(crate) fn check(call: &'static str, code: i32) -> Result<i32> {
        if code < 0 {
            return Err(Self::Krun { call, code });
        }
        Ok(code)
    }

    pub(crate) fn invalid(what: &'static str, detail: impl Into<String>) -> Self {
        Self::InvalidValue {
            what,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LibraryLoad { candidates, source } => {
                write!(f, "could not load libkrun; tried ")?;
                let mut found_on_disk = false;
                for (i, c) in candidates.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", c.display())?;
                    // dlopen's own text is generic on macOS, so distinguishing
                    // "absent" from "present but unloadable" has to happen here.
                    if c.exists() {
                        found_on_disk = true;
                        write!(f, " (present but unloadable)")?;
                    }
                }
                if let Some(source) = source {
                    write!(f, ". Last error: {source}")?;
                }
                if found_on_disk {
                    // Typically a wrong architecture, or a missing transitive
                    // dependency: libkrun links libepoxy and virglrenderer by
                    // absolute Homebrew path.
                    return write!(
                        f,
                        ". The file exists, so check its architecture and dependencies \
                         (`otool -L` / `ldd`), or point PIEBOX_LIBKRUN at a working build"
                    );
                }
                write!(
                    f,
                    ". Install it (`brew install libkrun/krun/libkrun`) or set PIEBOX_LIBKRUN"
                )
            }
            Self::Symbol { name, .. } => write!(
                f,
                "libkrun is missing symbol `{name}`; the installed version is too old for piebox"
            ),
            Self::Krun { call, code } => {
                // libkrun reports failures as a negated errno. `saturating_neg`
                // keeps Display panic-free even for a nonsensical i32::MIN.
                let errno = std::io::Error::from_raw_os_error(code.saturating_neg());
                write!(f, "{call} failed: {errno} (code {code})")
            }
            Self::InvalidValue { what, detail } => write!(f, "invalid {what}: {detail}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LibraryLoad { source, .. } => source.as_ref().map(|e| e as _),
            Self::Symbol { source, .. } => Some(source),
            Self::Krun { .. } | Self::InvalidValue { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn krun_error_renders_errno() {
        let msg = Error::Krun {
            call: "krun_set_vm_config",
            code: -22,
        }
        .to_string();
        assert!(msg.contains("krun_set_vm_config"), "{msg}");
        assert!(msg.contains("code -22"), "{msg}");
    }

    #[test]
    fn check_passes_through_non_negative() {
        assert_eq!(Error::check("krun_create_ctx", 7).unwrap(), 7);
        assert!(Error::check("krun_create_ctx", -1).is_err());
    }
}
