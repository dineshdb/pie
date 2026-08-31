//! The command-line surface itself.
//!
//! piebox follows docker where a flag means the same thing (`-v`, `-e`, `-w`,
//! `-m`), because that is the vocabulary anyone reaching for this already has.
//! These need no hypervisor: they check parsing and rejection only.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::process::{Command, Output};

fn piebox(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_piebox"))
        .args(args)
        .output()
        .expect("spawn piebox")
}

fn help() -> String {
    let output = piebox(&["run", "--help"]);
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_docker_flags_are_the_ones_documented() {
    let help = help();
    for expected in [
        "-v, --volume",
        "-e, --env",
        "-w, --workdir",
        "-m, --memory",
        "--cpus",
    ] {
        assert!(help.contains(expected), "missing {expected} in:\n{help}");
    }
}

/// `-m` is memory in docker, so it must not silently mean something else here.
#[test]
fn dash_m_is_memory_not_a_volume() {
    let help = help();
    let memory_line = help
        .lines()
        .find(|line| line.contains("-m, --memory"))
        .expect("memory flag");
    assert!(!memory_line.contains("volume"), "{memory_line}");

    // A size where a volume used to go is accepted as a size.
    let output = piebox(&["run", "/nonexistent", "-m", "2g", "--", "/bin/true"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("memory"),
        "2g should parse as a size: {stderr}"
    );
}

#[test]
fn memory_accepts_plain_mib_and_size_suffixes() {
    for good in ["512", "512m", "512M", "512MiB", "2g", "2G", "2GiB"] {
        let output = piebox(&["run", "/nonexistent", "-m", good, "--", "/bin/true"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // It must fail on the bogus rootfs, not on the size.
        assert!(
            !stderr.contains("invalid memory"),
            "{good} should be a valid size: {stderr}"
        );
    }

    for bad in ["", "abc", "1x", "512mb", "0", "4t"] {
        let output = piebox(&["run", "/nonexistent", "-m", bad, "--", "/bin/true"]);
        assert!(!output.status.success(), "{bad:?} must be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("memory") || stderr.contains("ram_mib"),
            "{bad:?} should be reported as a bad size: {stderr}"
        );
    }

    // A negative value looks like a flag, so clap refuses it before the size
    // parser sees it. Still rejected, just one layer earlier.
    let output = piebox(&["run", "/nonexistent", "-m", "-4", "--", "/bin/true"]);
    assert!(!output.status.success(), "-4 must be rejected");
}

/// A bare number is MiB, which is where piebox deliberately parts company with
/// docker (docker reads bytes). Being wrong about this by a factor of a million
/// would be a confusing way to fail, so it is pinned.
#[test]
fn a_bare_memory_number_is_mib() {
    // 64 as bytes would be far below any workable floor and would be rejected;
    // as MiB it is a legal, if small, VM.
    let output = piebox(&["run", "/nonexistent", "-m", "256", "--", "/bin/true"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("image"),
        "256 should be read as MiB and get as far as resolving the image: {stderr}"
    );
}

#[test]
fn the_old_flag_names_are_gone() {
    for retired in ["--mount", "--ram", "--vcpus"] {
        let output = piebox(&["run", "/nonexistent", retired, "1", "--", "/bin/true"]);
        assert!(!output.status.success(), "{retired} should no longer exist");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("unexpected argument") || stderr.contains("--help"),
            "{retired}: {stderr}"
        );
    }
}

// --- The IMAGE positional -------------------------------------------------

#[test]
fn image_is_a_required_positional() {
    let output = piebox(&["run", "--", "/bin/true"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("<IMAGE>"), "{stderr}");

    let help = help();
    assert!(help.contains("<IMAGE>"), "{help}");
}

/// A path when it contains `/`, a container name otherwise. Deterministic by
/// shape: probing the disk would make the same argument mean different things
/// depending on the working directory.
#[test]
fn an_image_is_a_path_only_when_it_looks_like_one() {
    // Contains `/`, so it is a path — and reported as a path.
    let output = piebox(&["run", "/nonexistent/rootfs", "--", "/bin/true"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid image"), "{stderr}");
    assert!(stderr.contains("/nonexistent/rootfs"), "{stderr}");

    // No `/`, so it is a container name, and the error comes from the store.
    let output = piebox(&["run", "definitely-not-a-container", "--", "/bin/true"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("buildah") || stderr.contains("container storage"),
        "a bare name should be looked up as a container: {stderr}"
    );
}

/// A typo'd path is usually still a directory, and booting it would fail deep
/// inside the guest rather than at the argument that was wrong.
#[test]
fn a_directory_that_is_not_a_root_filesystem_is_refused() {
    let dir = std::env::temp_dir().join(format!("piebox-not-rootfs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let output = piebox(&["run", dir.to_str().expect("utf8"), "--", "/bin/true"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("root filesystem"), "{stderr}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_image_name_that_could_be_a_buildah_flag_is_refused() {
    for bad in ["-rf", "--storage-driver=overlay"] {
        let output = piebox(&["run", bad, "--", "/bin/true"]);
        assert!(!output.status.success(), "{bad} must be refused");
    }
}

/// Flags are checked before the image is resolved, so a typo is reported as
/// itself rather than behind an image error — and resolving an image mounts a
/// container, which should not happen on the way to rejecting a bad flag.
#[test]
fn a_bad_flag_is_reported_before_the_image_is_resolved() {
    let output = piebox(&[
        "run",
        "/nonexistent/rootfs",
        "-v",
        "not-a-volume-spec",
        "--",
        "/bin/true",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mount"),
        "the volume should be blamed: {stderr}"
    );
    assert!(!stderr.contains("invalid image"), "{stderr}");
}
