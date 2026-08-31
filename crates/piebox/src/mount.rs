//! Host directories exposed to a guest.
//!
//! piebox's own mounts: a host directory becomes a virtio-fs device with a tag,
//! and the guest supervisor mounts that tag at a path inside the guest. The
//! host cannot mount it itself — libkrun's API for that
//! (`krun_set_mapped_volumes`) is marked "NO LONGER SUPPORTED" — so mounts are
//! only available on the supervised path.

use crate::error::{Error, Result};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

/// virtio-fs tags are 36 bytes (`virtio_fs_config.tag[36]`). Generated tags are
/// `piebox` plus a decimal index, so they cannot come close, and they cannot
/// collide with libkrun's own root tag (`/dev/root`).
const MAX_TAG: usize = 36;

/// Guest paths piebox needs for itself.
const RESERVED_TARGETS: [&str; 1] = [crate::supervisor::GUEST_STAGING_DIR];

/// One host directory, exposed to the guest.
///
/// Deserialized through [`Mount::new`], because a spec arriving from another
/// process is not more trustworthy than a command line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "Wire")]
pub struct Mount {
    /// Directory on the host.
    pub host_path: PathBuf,
    /// Absolute, normalized path inside the guest.
    pub guest_path: PathBuf,
    /// Expose (and mount) read-only.
    pub read_only: bool,
}

/// Deserialization shape for [`Mount`]. Validated on the way in.
#[derive(serde::Deserialize)]
struct Wire {
    host_path: PathBuf,
    guest_path: PathBuf,
    read_only: bool,
}

impl TryFrom<Wire> for Mount {
    type Error = Error;

    fn try_from(wire: Wire) -> Result<Self> {
        Self::new(wire.host_path, wire.guest_path, wire.read_only)
    }
}

impl Mount {
    /// Builds a mount, checking everything that can be checked from the host.
    ///
    /// # Errors
    /// Fails when the host path is missing or not a directory, or the guest
    /// path is not an absolute, normalized path that piebox is willing to
    /// mount over.
    pub fn new(
        host_path: impl Into<PathBuf>,
        guest_path: impl Into<PathBuf>,
        read_only: bool,
    ) -> Result<Self> {
        let host_path = host_path.into();
        let guest_path = guest_path.into();

        // Canonicalized so a relative `-v .:/work` means what the caller
        // saw, and so the device points at a stable path.
        let host_path = host_path.canonicalize().map_err(|err| {
            Error::invalid("mount", format!("host path {}: {err}", host_path.display()))
        })?;
        if !host_path.is_dir() {
            return Err(Error::invalid(
                "mount",
                format!("host path {} is not a directory", host_path.display()),
            ));
        }

        let guest_path = normalize_guest_path(&guest_path)?;
        Ok(Self {
            host_path,
            guest_path,
            read_only,
        })
    }
}

/// Checks and normalizes a guest mount point.
///
/// `..` and `.` are rejected rather than resolved: `/bar/..` is `/`, so
/// accepting them would let a three-character path replace the guest's root
/// filesystem and slip past any comparison done on the written form.
fn normalize_guest_path(guest_path: &Path) -> Result<PathBuf> {
    if !guest_path.is_absolute() {
        return Err(Error::invalid(
            "mount",
            format!("guest path {} must be absolute", guest_path.display()),
        ));
    }

    let mut normalized = PathBuf::from("/");
    for component in guest_path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => normalized.push(part),
            // `.` never reaches here: `Path::components` drops it, and `/./x`
            // really is `/x`. `..` is preserved, and is the dangerous one.
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(Error::invalid(
                    "mount",
                    format!("guest path {} must not contain `..`", guest_path.display()),
                ));
            }
            Component::Prefix(_) => {
                return Err(Error::invalid(
                    "mount",
                    format!("guest path {} is not a Unix path", guest_path.display()),
                ));
            }
        }
    }

    if normalized == Path::new("/") {
        return Err(Error::invalid(
            "mount",
            "guest path / is the root filesystem and cannot be replaced",
        ));
    }
    for reserved in RESERVED_TARGETS {
        let reserved = Path::new("/").join(reserved);
        if normalized == reserved || normalized.starts_with(&reserved) {
            return Err(Error::invalid(
                "mount",
                format!("guest path {} is reserved by piebox", normalized.display()),
            ));
        }
    }
    Ok(normalized)
}

impl FromStr for Mount {
    type Err = Error;

    /// Parses `HOST:GUEST` or `HOST:GUEST:ro`.
    fn from_str(raw: &str) -> Result<Self> {
        let parts: Vec<&str> = raw.split(':').collect();
        let (host, guest, read_only) = match parts.as_slice() {
            [host, guest] => (*host, *guest, false),
            [host, guest, "ro"] => (*host, *guest, true),
            [host, guest, "rw"] => (*host, *guest, false),
            _ => {
                return Err(Error::invalid(
                    "mount",
                    format!("{raw:?} is not HOST:GUEST or HOST:GUEST:ro"),
                ));
            }
        };
        if host.is_empty() || guest.is_empty() {
            return Err(Error::invalid(
                "mount",
                format!("{raw:?} has an empty path"),
            ));
        }
        Self::new(host, guest, read_only)
    }
}

impl fmt::Display for Mount {
    /// Renders the `HOST:GUEST[:ro]` form. Note this does not round-trip for a
    /// host path containing a colon.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}{}",
            self.host_path.display(),
            self.guest_path.display(),
            if self.read_only { ":ro" } else { "" }
        )
    }
}

/// Pairs each mount with the virtio-fs tag identifying its device.
///
/// The single definition of that correspondence. The host attaches devices by
/// tag and the guest is told which tag to mount where, from two different
/// places (and in two different processes), so deriving the pairing twice would
/// be a standing invitation to mount a read-write device where `:ro` was asked
/// for.
pub fn tagged(mounts: &[Mount]) -> Vec<(String, &Mount)> {
    mounts
        .iter()
        .enumerate()
        .map(|(index, mount)| {
            let tag = format!("piebox{index}");
            debug_assert!(tag.len() <= MAX_TAG, "tag must fit virtio-fs: {tag}");
            (tag, mount)
        })
        .collect()
}

/// Rejects mounts that would shadow one another.
///
/// Nesting is refused rather than ordered: mounting `/work` after `/work/inner`
/// hides the inner one completely, and mounting it before means the inner mount
/// point gets created inside the *caller's* host directory. Both are surprises
/// worth refusing instead of explaining.
///
/// # Errors
/// Fails when two guest paths are equal, or one contains the other.
pub fn check_collisions(mounts: &[Mount]) -> Result<()> {
    for (index, mount) in mounts.iter().enumerate() {
        for earlier in mounts.get(..index).unwrap_or_default() {
            let (a, b) = (&earlier.guest_path, &mount.guest_path);
            if a == b {
                return Err(Error::invalid(
                    "mount",
                    format!(
                        "{} is mounted twice ({} and {})",
                        b.display(),
                        earlier.host_path.display(),
                        mount.host_path.display()
                    ),
                ));
            }
            if a.starts_with(b) || b.starts_with(a) {
                return Err(Error::invalid(
                    "mount",
                    format!(
                        "{} and {} are nested; one would shadow the other",
                        a.display(),
                        b.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Mount points piebox created inside the guest rootfs, removed on drop.
///
/// The host creates them, not the guest: the rootfs is a host directory, so the
/// host can both create and clean up, while a guest-created directory would be
/// left behind in the image forever.
///
/// Not airtight under concurrency, deliberately. When two runs share a target,
/// only one of them created the directory and only that one removes it; if the
/// other's guest had to recreate it (because the first finished first), it stays
/// in the image. Leaving an empty directory behind is a much better outcome than
/// deleting one another run is using, so the race resolves that way on purpose.
#[derive(Debug, Default)]
pub struct MountPoints {
    created: Vec<PathBuf>,
}

impl MountPoints {
    /// Creates any mount point that the image does not already have.
    ///
    /// # Errors
    /// Fails when a mount point cannot be created, or exists as a file.
    pub fn create(rootfs: &Path, mounts: &[Mount]) -> Result<Self> {
        let mut created = Vec::new();
        for mount in mounts {
            let relative = mount
                .guest_path
                .strip_prefix("/")
                .map_err(|_| Error::invalid("mount", "guest path must be absolute"))?;
            let host_side = rootfs.join(relative);
            if host_side.is_dir() {
                continue;
            }
            if host_side.exists() {
                return Err(Error::invalid(
                    "mount",
                    format!(
                        "{} already exists in the guest and is not a directory",
                        mount.guest_path.display()
                    ),
                ));
            }
            // One component at a time, recording each: `create_dir_all` would
            // make the parents too, and then only the leaf would be cleaned up,
            // leaving `/cleanup-1234/` behind in the image forever.
            let mut path = rootfs.to_path_buf();
            for component in relative.components() {
                path.push(component);
                if path.is_dir() {
                    continue;
                }
                match std::fs::create_dir(&path) {
                    Ok(()) => created.push(path.clone()),
                    // Another run created it between the check and here. Not
                    // recorded, so this one will not remove a directory it does
                    // not own — leaving one behind is the safer mistake.
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(err) => {
                        return Err(Error::Command {
                            program: "piebox",
                            detail: format!(
                                "could not create mount point {}: {err}",
                                path.display()
                            ),
                        });
                    }
                }
            }
        }
        Ok(Self { created })
    }
}

impl Drop for MountPoints {
    fn drop(&mut self) {
        // Deepest first, and only if empty: anything the guest left behind is
        // the caller's data, not piebox's to delete.
        self.created
            .sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in &self.created {
            let _ = std::fs::remove_dir(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("piebox-mount-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn parses_host_and_guest_paths() {
        let dir = temp_dir("basic");
        let mount: Mount = format!("{}:/work", dir.display()).parse().expect("parse");
        assert_eq!(mount.guest_path, Path::new("/work"));
        assert!(!mount.read_only);
        assert_eq!(mount.host_path, dir.canonicalize().expect("canonical"));
    }

    #[test]
    fn parses_the_read_only_suffix() {
        let dir = temp_dir("ro");
        let mount: Mount = format!("{}:/work:ro", dir.display())
            .parse()
            .expect("parse");
        assert!(mount.read_only);
        let mount: Mount = format!("{}:/work:rw", dir.display())
            .parse()
            .expect("parse");
        assert!(!mount.read_only);
    }

    #[test]
    fn a_relative_host_path_is_resolved() {
        let mount = Mount::new(".", "/work", false).expect("cwd is a directory");
        assert!(mount.host_path.is_absolute(), "{mount}");
    }

    #[test]
    fn rejects_malformed_specifications() {
        let dir = temp_dir("bad");
        let host = dir.display().to_string();
        for bad in [
            String::new(),
            "onlyonepath".to_string(),
            format!("{host}:/work:maybe"),
            format!("{host}:/work:ro:extra"),
            format!(":{host}"),
            format!("{host}:"),
        ] {
            assert!(bad.parse::<Mount>().is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn rejects_a_missing_or_non_directory_host_path() {
        assert!(Mount::new("/nonexistent/piebox-mount", "/work", false).is_err());

        let dir = temp_dir("file");
        let file = dir.join("a-file");
        std::fs::write(&file, b"x").expect("write");
        assert!(Mount::new(&file, "/work", false).is_err());
    }

    /// `/bar/..` is `/`, so a guest path written with `..` could replace the
    /// root filesystem while looking like an ordinary directory.
    #[test]
    fn rejects_traversal_in_the_guest_path() {
        let dir = temp_dir("traversal");
        for bad in ["/bar/..", "/..", "/a/../b", "work", "/"] {
            assert!(
                Mount::new(&dir, bad, false).is_err(),
                "{bad:?} must be rejected"
            );
        }
        assert!(Mount::new(&dir, "/work", false).is_ok());

        // `.` and a trailing slash are merely untidy: they normalize to the
        // same path, so they are accepted rather than refused.
        for (written, expected) in [("/./work", "/work"), ("/a/./b", "/a/b")] {
            assert_eq!(
                Mount::new(&dir, written, false)
                    .expect("should normalize")
                    .guest_path,
                Path::new(expected),
                "{written}"
            );
        }
        assert_eq!(
            Mount::new(&dir, "/work/", false)
                .expect("trailing slash")
                .guest_path,
            Path::new("/work")
        );
    }

    #[test]
    fn rejects_targets_piebox_needs_for_itself() {
        let dir = temp_dir("reserved");
        for reserved in ["/.piebox", "/.piebox/inner"] {
            let err = Mount::new(&dir, reserved, false).expect_err("must be reserved");
            assert!(err.to_string().contains("reserved"), "{err}");
        }
    }

    #[test]
    fn tags_are_unique_and_short_enough_for_virtiofs() {
        let dir = temp_dir("tags");
        let mounts: Vec<Mount> = (0..16)
            .map(|i| Mount::new(&dir, format!("/m{i}"), false).expect("mount"))
            .collect();
        let tags: Vec<String> = tagged(&mounts).into_iter().map(|(tag, _)| tag).collect();
        for tag in &tags {
            assert!(tag.len() <= MAX_TAG, "{tag}");
            assert!(tag.is_ascii(), "{tag}");
        }
        let mut sorted = tags.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), tags.len(), "tags must be unique");
    }

    #[test]
    fn tagging_pairs_each_tag_with_its_own_mount() {
        let one = temp_dir("pair-one");
        let two = temp_dir("pair-two");
        let mounts = vec![
            Mount::new(&one, "/one", false).expect("first"),
            Mount::new(&two, "/two", true).expect("second"),
        ];
        let pairs = tagged(&mounts);
        assert_eq!(pairs[0].1.guest_path, Path::new("/one"));
        assert!(!pairs[0].1.read_only);
        assert_eq!(pairs[1].1.guest_path, Path::new("/two"));
        assert!(
            pairs[1].1.read_only,
            "the ro flag must follow its own mount"
        );
        assert_ne!(pairs[0].0, pairs[1].0);
    }

    #[test]
    fn the_same_guest_path_twice_is_refused() {
        let one = temp_dir("collide-one");
        let two = temp_dir("collide-two");
        let mounts = vec![
            Mount::new(&one, "/work", false).expect("first"),
            Mount::new(&two, "/work", false).expect("second"),
        ];
        let err = check_collisions(&mounts).expect_err("a shadowed mount must be refused");
        assert!(err.to_string().contains("mounted twice"), "{err}");

        // Different targets from the same source are fine.
        let fine = vec![
            Mount::new(&one, "/work", false).expect("first"),
            Mount::new(&one, "/also", true).expect("second"),
        ];
        check_collisions(&fine).expect("distinct targets");
    }

    /// Nesting is not a collision by equality, but one mount still hides the
    /// other — in whichever order they are applied.
    #[test]
    fn nested_guest_paths_are_refused_in_either_order() {
        let outer = temp_dir("nest-outer");
        let inner = temp_dir("nest-inner");
        for (first, second) in [("/work", "/work/inner"), ("/work/inner", "/work")] {
            let mounts = vec![
                Mount::new(&outer, first, false).expect("first"),
                Mount::new(&inner, second, false).expect("second"),
            ];
            let err = check_collisions(&mounts).expect_err("nesting must be refused");
            assert!(err.to_string().contains("nested"), "{err}");
        }
        // A shared prefix that is not a path prefix is fine.
        let fine = vec![
            Mount::new(&outer, "/work", false).expect("first"),
            Mount::new(&inner, "/workshop", false).expect("second"),
        ];
        check_collisions(&fine).expect("sibling directories");
    }

    /// A spec crossing a process boundary must be validated like a command line.
    #[test]
    fn deserialization_goes_through_validation() {
        let dir = temp_dir("serde");
        let mount = Mount::new(&dir, "/work", true).expect("mount");
        let json = serde_json::to_string(&mount).expect("serialize");
        assert_eq!(
            serde_json::from_str::<Mount>(&json).expect("round trip"),
            mount
        );

        for bad in [
            json.replace("/work", "work"),
            json.replace("/work", "/bar/.."),
            json.replace("/work", "/.piebox"),
            json.replace(
                dir.canonicalize()
                    .expect("canonical")
                    .to_str()
                    .expect("utf8"),
                "/nonexistent/piebox-serde",
            ),
        ] {
            assert!(
                serde_json::from_str::<Mount>(&bad).is_err(),
                "must be rejected: {bad}"
            );
        }
    }

    #[test]
    fn mount_points_are_created_and_cleaned_up() {
        let host = temp_dir("points-host");
        let rootfs = temp_dir("points-rootfs");
        let mounts = vec![Mount::new(&host, "/created/deeper", false).expect("mount")];

        let created = rootfs.join("created");
        {
            let _points = MountPoints::create(&rootfs, &mounts).expect("create");
            assert!(created.join("deeper").is_dir());
        }
        // Both levels: recording only the leaf would leave the parent behind.
        assert!(
            !created.exists(),
            "piebox should remove every directory it created, parents included"
        );
    }

    #[test]
    fn an_existing_mount_point_is_left_alone() {
        let host = temp_dir("points-existing-host");
        let rootfs = temp_dir("points-existing-rootfs");
        let existing = rootfs.join("existing");
        std::fs::create_dir_all(&existing).expect("pre-create");
        let mounts = vec![Mount::new(&host, "/existing", false).expect("mount")];
        {
            let _points = MountPoints::create(&rootfs, &mounts).expect("create");
        }
        assert!(
            existing.is_dir(),
            "a directory piebox did not create must survive"
        );
    }

    #[test]
    fn a_mount_point_blocked_by_a_file_is_reported() {
        let host = temp_dir("points-file-host");
        let rootfs = temp_dir("points-file-rootfs");
        std::fs::write(rootfs.join("blocked"), b"x").expect("write");
        let mounts = vec![Mount::new(&host, "/blocked", false).expect("mount")];
        let err = MountPoints::create(&rootfs, &mounts).expect_err("a file is not a mount point");
        assert!(err.to_string().contains("not a directory"), "{err}");
    }

    #[test]
    fn display_renders_the_specification() {
        let dir = temp_dir("display");
        let mount = Mount::new(&dir, "/work", true).expect("mount");
        let text = mount.to_string();
        assert!(text.ends_with(":/work:ro"), "{text}");
        assert_eq!(text.parse::<Mount>().expect("re-parse"), mount);
    }
}
