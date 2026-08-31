//! Finding the host directory to expose as a guest root filesystem.
//!
//! piebox boots an OCI image the way krunvm does: buildah materialises the
//! image into a plain directory, and that directory is handed to libkrun as a
//! virtio-fs root. Nothing is copied or converted, so the same store krunvm
//! already populated can be booted directly.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Overrides for the container store location.
pub const ENV_STORAGE_ROOT: &str = "PIEBOX_STORAGE_ROOT";
pub const ENV_STORAGE_RUNROOT: &str = "PIEBOX_STORAGE_RUNROOT";

/// Where krunvm keeps its store on macOS. Container storage needs a
/// case-sensitive filesystem, which the default APFS volume is not, so this is
/// a separate case-sensitive volume rather than a directory under $HOME.
const DEFAULT_STORAGE_VOLUME: &str = "/Volumes/krunvm";

/// A buildah/containers store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerStorage {
    pub root: PathBuf,
    pub runroot: PathBuf,
}

impl ContainerStorage {
    pub fn new(root: impl Into<PathBuf>, runroot: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            runroot: runroot.into(),
        }
    }

    /// Reads the store location from the environment, else krunvm's default.
    pub fn from_env() -> Self {
        let volume = Path::new(DEFAULT_STORAGE_VOLUME);
        let root =
            std::env::var_os(ENV_STORAGE_ROOT).map_or_else(|| volume.join("root"), PathBuf::from);
        let runroot = std::env::var_os(ENV_STORAGE_RUNROOT)
            .map_or_else(|| volume.join("runroot"), PathBuf::from);
        Self::new(root, runroot)
    }

    /// Arguments that point buildah at this store.
    fn args(&self) -> [&std::ffi::OsStr; 4] {
        [
            std::ffi::OsStr::new("--root"),
            self.root.as_os_str(),
            std::ffi::OsStr::new("--runroot"),
            self.runroot.as_os_str(),
        ]
    }
}

/// Mounts `container` and returns the host directory holding its root filesystem.
///
/// Returns the same path when already mounted, but note it is *not* free to
/// call repeatedly: containers/storage keeps a persistent mount reference count
/// and this increments it every time. With the `vfs` driver that is only
/// bookkeeping; with `overlay` a non-zero count pins a real kernel mount, so a
/// long-running caller should eventually `buildah umount` what it mounted.
///
/// The guest gets this directory read-write — `krun_set_root` offers no
/// read-only option — so a guest writes straight through into the host's
/// container store.
///
/// # Errors
/// Fails when buildah is missing, the store does not exist, the container name
/// is unusable, the container is unknown, or the reported path is not a
/// directory.
pub fn container_rootfs(storage: &ContainerStorage, container: &str) -> Result<PathBuf> {
    validate_container_name(container)?;
    if !storage.root.is_dir() {
        return Err(Error::invalid(
            "container storage",
            format!(
                "{} does not exist; set {ENV_STORAGE_ROOT} (a case-sensitive volume is required)",
                storage.root.display()
            ),
        ));
    }

    let output = Command::new("buildah")
        .args(storage.args())
        .arg("mount")
        // Without the separator buildah reads a name like `--storage-driver=x`
        // as one of its own flags.
        .arg("--")
        .arg(container)
        .output()
        .map_err(|err| Error::Command {
            program: "buildah",
            detail: format!("could not run it: {err}"),
        })?;

    if !output.status.success() {
        return Err(Error::Command {
            program: "buildah",
            detail: format!(
                "mount {container} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }

    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string());
    if !path.is_dir() {
        return Err(Error::Command {
            program: "buildah",
            detail: format!("reported rootfs {} is not a directory", path.display()),
        });
    }
    tracing::debug!(container, rootfs = %path.display(), "resolved container rootfs");
    Ok(path)
}

/// Rejects container names that buildah would misread or that are not names.
fn validate_container_name(container: &str) -> Result<()> {
    let valid = !container.is_empty()
        && container
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && !container.starts_with('-');
    if !valid {
        return Err(Error::invalid(
            "container",
            format!("{container:?} is not a container name"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_storage_lives_on_the_case_sensitive_volume() {
        // Env may be set on this machine, so check the fallback directly.
        let storage = ContainerStorage::new(
            Path::new(DEFAULT_STORAGE_VOLUME).join("root"),
            Path::new(DEFAULT_STORAGE_VOLUME).join("runroot"),
        );
        assert!(storage.root.ends_with("root"));
        assert!(storage.runroot.ends_with("runroot"));
    }

    #[test]
    fn storage_args_are_in_buildah_order() {
        let storage = ContainerStorage::new("/s/root", "/s/runroot");
        let args: Vec<_> = storage
            .args()
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, ["--root", "/s/root", "--runroot", "/s/runroot"]);
    }

    #[test]
    fn container_names_that_buildah_would_misread_are_rejected() {
        for name in [
            "",
            "-rf",
            "--storage-driver=overlay",
            "a b",
            "a;rm -rf /",
            "a$(id)",
        ] {
            assert!(
                validate_container_name(name).is_err(),
                "{name:?} must be rejected"
            );
        }
        for name in ["ubuntu-working-container", "piebox_1.2-base"] {
            assert!(validate_container_name(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn a_missing_store_is_reported_before_buildah_runs() {
        let storage = ContainerStorage::new("/nonexistent/piebox-store", "/nonexistent/piebox-run");
        let err = container_rootfs(&storage, "whatever").expect_err("must fail");
        assert!(
            matches!(
                err,
                Error::InvalidValue {
                    what: "container storage",
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains(ENV_STORAGE_ROOT), "{err}");
    }
}
