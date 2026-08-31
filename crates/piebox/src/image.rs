//! What a guest boots from.
//!
//! One argument covers both ways of naming it, the way docker's `IMAGE`
//! positional does: a buildah container in the local store, or a host directory
//! that already holds a root filesystem.
//!
//! The two are told apart by shape, not by probing the disk: anything
//! containing `/` is a path, everything else is a container name. Container
//! names cannot contain `/` in the first place, so the rule is unambiguous —
//! and a rule that guessed based on what happens to exist would resolve the
//! same argument differently from one directory to the next.

use crate::error::{Error, Result};
use crate::rootfs::ContainerStorage;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Overrides the image when none is given on the command line.
pub const ENV_IMAGE: &str = "PIEBOX_IMAGE";

/// Entries that mark a directory as plausibly a root filesystem.
///
/// A typo'd path is usually still a directory, and booting it would fail deep
/// inside the guest ("Couldn't execute ...") rather than at the argument that
/// was wrong.
const ROOTFS_MARKERS: [&str; 4] = ["bin", "usr", "sbin", "lib"];

/// A root filesystem to boot, named either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    /// A host directory holding a root filesystem.
    Path(PathBuf),
    /// A container in the local buildah store.
    Container(String),
}

impl Image {
    /// Resolves the image to a host directory the guest can boot.
    ///
    /// # Errors
    /// Fails when a path is not a usable root filesystem, or a container cannot
    /// be found and mounted.
    pub fn rootfs(&self, storage: &ContainerStorage) -> Result<PathBuf> {
        match self {
            Self::Path(path) => resolve_path(path),
            Self::Container(name) => crate::rootfs::container_rootfs(storage, name),
        }
    }
}

fn resolve_path(path: &Path) -> Result<PathBuf> {
    let resolved = path
        .canonicalize()
        .map_err(|err| Error::invalid("image", format!("{}: {err}", path.display())))?;
    if !resolved.is_dir() {
        return Err(Error::invalid(
            "image",
            format!("{} is not a directory", resolved.display()),
        ));
    }
    if !ROOTFS_MARKERS
        .iter()
        .any(|marker| resolved.join(marker).exists())
    {
        return Err(Error::invalid(
            "image",
            format!(
                "{} does not look like a root filesystem (no {})",
                resolved.display(),
                ROOTFS_MARKERS.join(", ")
            ),
        ));
    }
    Ok(resolved)
}

impl FromStr for Image {
    type Err = Error;

    fn from_str(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(Error::invalid("image", "is empty"));
        }
        // A path is anything that says it is one. Container names cannot
        // contain `/`, so nothing is lost by the rule.
        if raw.contains('/') {
            return Ok(Self::Path(PathBuf::from(raw)));
        }
        validate_container_name(raw)?;
        Ok(Self::Container(raw.to_string()))
    }
}

impl fmt::Display for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(path) => write!(f, "{}", path.display()),
            Self::Container(name) => write!(f, "{name}"),
        }
    }
}

/// Rejects container names buildah would misread, or that are not names.
///
/// Notably a leading `-`, which buildah would take for one of its own flags.
fn validate_container_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && !name.starts_with('-');
    if !valid {
        return Err(Error::invalid(
            "image",
            format!("{name:?} is neither a path nor a container name"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anything_with_a_slash_is_a_path() {
        for raw in ["/abs/rootfs", "./rootfs", "../rootfs", "a/b", "/"] {
            assert!(
                matches!(raw.parse::<Image>(), Ok(Image::Path(_))),
                "{raw} should be a path"
            );
        }
    }

    #[test]
    fn a_bare_name_is_a_container() {
        for raw in ["ubuntu-working-container", "piebox_1.2-base", "img"] {
            assert_eq!(
                raw.parse::<Image>().expect("valid name"),
                Image::Container(raw.to_string())
            );
        }
    }

    /// The rule is by shape, so a directory in the working directory has to be
    /// written as a path — otherwise the same argument would mean different
    /// things depending on where it was run.
    #[test]
    fn a_bare_name_is_never_treated_as_a_relative_path() {
        assert_eq!(
            "rootfs".parse::<Image>().expect("valid name"),
            Image::Container("rootfs".to_string())
        );
    }

    #[test]
    fn rejects_names_that_are_neither() {
        for bad in [
            "",
            "  ",
            "-rf",
            "--storage-driver=overlay",
            "a b",
            "a;id",
            "a$(id)",
        ] {
            assert!(bad.parse::<Image>().is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn a_path_that_is_not_a_root_filesystem_is_reported() {
        let dir = std::env::temp_dir().join(format!("piebox-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let storage = ContainerStorage::from_env();

        let image = Image::Path(dir.clone());
        let err = image
            .rootfs(&storage)
            .expect_err("an empty dir is not a rootfs");
        assert!(err.to_string().contains("root filesystem"), "{err}");

        // With something that marks it as one, it resolves.
        std::fs::create_dir_all(dir.join("bin")).expect("bin");
        let resolved = image.rootfs(&storage).expect("now plausible");
        assert_eq!(resolved, dir.canonicalize().expect("canonical"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_path_names_itself_in_the_error() {
        let image = Image::Path(PathBuf::from("/nonexistent/piebox-image"));
        let err = image
            .rootfs(&ContainerStorage::from_env())
            .expect_err("must fail");
        assert!(
            err.to_string().contains("/nonexistent/piebox-image"),
            "{err}"
        );
    }

    #[test]
    fn display_round_trips() {
        for raw in ["/abs/rootfs", "ubuntu-working-container"] {
            let image: Image = raw.parse().expect("parse");
            assert_eq!(image.to_string(), raw);
        }
    }
}
