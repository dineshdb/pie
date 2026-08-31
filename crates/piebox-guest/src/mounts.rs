//! Mounting the virtio-fs devices the host attached.
//!
//! The host can only attach devices; mounting them is the guest's job, because
//! libkrun's own API for it (`krun_set_mapped_volumes`) is marked "NO LONGER
//! SUPPORTED". Applied on every request and idempotent, so the supervisor keeps
//! no state and a reconnecting host does not have to remember what it asked for.

use piebox_proto::Mount;
use std::ffi::CString;
use std::path::Path;

/// Mounts anything in `mounts` that is not mounted yet.
///
/// # Errors
/// Returns a message naming the mount that could not be set up. A command must
/// never run with a filesystem it expected missing: it would read an empty
/// directory and report success on nothing.
pub fn ensure(mounts: &[Mount]) -> Result<(), String> {
    if mounts.is_empty() {
        return Ok(());
    }
    let existing = existing_mounts();
    for mount in mounts {
        let target = normalize(&mount.target);
        match existing.iter().find(|entry| entry.target == target) {
            // Already ours, with the same access. Nothing to do.
            Some(entry) if entry.is(mount) => continue,
            // Something else lives here. Mounting over it would hide whatever
            // the guest needs that path for, and skipping would run the command
            // against the wrong filesystem, so neither is safe to guess at.
            Some(entry) => {
                return Err(format!(
                    "cannot mount {} at {}: {} is already mounted there{}",
                    mount.tag,
                    mount.target,
                    entry.fstype,
                    if entry.source == mount.tag {
                        ", with different options"
                    } else {
                        ""
                    }
                ));
            }
            None => mount_one(mount)?,
        }
    }
    Ok(())
}

/// One line of `/proc/mounts`.
struct Entry {
    source: String,
    target: String,
    fstype: String,
    read_only: bool,
}

impl Entry {
    /// Whether this entry is already exactly what `mount` asks for.
    fn is(&self, mount: &Mount) -> bool {
        self.source == mount.tag && self.fstype == "virtiofs" && self.read_only == mount.read_only
    }
}

/// Reads the kernel's view of what is mounted.
fn existing_mounts() -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let target = fields.next()?;
            let fstype = fields.next()?;
            let options = fields.next().unwrap_or_default();
            Some(Entry {
                source: unescape(source),
                target: normalize(&unescape(target)),
                fstype: fstype.to_string(),
                read_only: options.split(',').any(|option| option == "ro"),
            })
        })
        .collect()
}

/// Undoes the octal escaping the kernel applies to `/proc/mounts` fields.
///
/// Space, tab, newline and backslash arrive as `\040` and friends, so a target
/// like `/my work` would never compare equal to the requested path and would be
/// mounted again on every request.
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let digits: String = chars.clone().take(3).collect();
        match u8::from_str_radix(&digits, 8) {
            Ok(byte) if digits.len() == 3 => {
                out.push(char::from(byte));
                for _ in 0..3 {
                    chars.next();
                }
            }
            // A lone backslash is a legal character in a path.
            _ => out.push(c),
        }
    }
    out
}

/// Drops a trailing slash, which the kernel does not report.
fn normalize(target: &str) -> String {
    let trimmed = target.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn mount_one(mount: &Mount) -> Result<(), String> {
    let target = Path::new(&mount.target);
    if !target.is_absolute() {
        return Err(format!("mount target {} is not absolute", mount.target));
    }
    // The host normally creates the mount point, because it owns the rootfs
    // directory and can therefore also clean it up afterwards. Creating it here
    // too is not redundant: two runs sharing a target both see it, and whichever
    // finishes first removes it — possibly while this guest is still booting.
    if !target.is_dir() {
        std::fs::create_dir_all(target)
            .map_err(|err| format!("could not create mount point {}: {err}", mount.target))?;
    }

    let source = CString::new(mount.tag.as_bytes())
        .map_err(|_| format!("mount tag {:?} contains a NUL byte", mount.tag))?;
    let target_c = CString::new(mount.target.as_bytes())
        .map_err(|_| format!("mount target {:?} contains a NUL byte", mount.target))?;

    let flags = if mount.read_only { libc::MS_RDONLY } else { 0 };

    // SAFETY: both strings are NUL-terminated and outlive the call, the fstype
    // is a literal C string, and NULL data is what virtiofs expects (the tag is
    // the source).
    let result = unsafe {
        libc::mount(
            source.as_ptr(),
            target_c.as_ptr(),
            c"virtiofs".as_ptr(),
            flags,
            std::ptr::null(),
        )
    };
    if result != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!(
            "could not mount {} at {}: {err}",
            mount.tag, mount.target
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octal_escapes_are_undone() {
        assert_eq!(unescape("/my\\040work"), "/my work");
        assert_eq!(unescape("/a\\011b"), "/a\tb");
        assert_eq!(unescape("/plain"), "/plain");
        // A backslash that is not an escape stays put.
        assert_eq!(unescape("/back\\slash"), "/back\\slash");
        assert_eq!(unescape("/trailing\\"), "/trailing\\");
    }

    #[test]
    fn targets_are_compared_without_a_trailing_slash() {
        assert_eq!(normalize("/work/"), "/work");
        assert_eq!(normalize("/work"), "/work");
        assert_eq!(normalize("/"), "/");
    }

    #[test]
    fn an_entry_matches_only_the_same_device_and_access() {
        let wanted = Mount {
            tag: "piebox0".to_string(),
            target: "/work".to_string(),
            read_only: true,
        };
        let same = Entry {
            source: "piebox0".to_string(),
            target: "/work".to_string(),
            fstype: "virtiofs".to_string(),
            read_only: true,
        };
        assert!(same.is(&wanted));

        // Read-write where read-only was asked for must not count as done, or a
        // later request could never tighten access.
        let writable = Entry {
            read_only: false,
            ..same
        };
        assert!(!writable.is(&wanted));

        let other_fs = Entry {
            source: "tmpfs".to_string(),
            fstype: "tmpfs".to_string(),
            ..writable
        };
        assert!(!other_fs.is(&wanted));
    }
}
