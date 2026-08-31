//! Does piebox actually boot a guest, and does the workload's outcome survive
//! the trip back?
//!
//! These need a real hypervisor, so they skip when the host cannot provide one
//! (no libkrun, no container store, no buildah). On macOS they sign the test
//! binary first: `hv_vm_create` refuses to run without the hypervisor
//! entitlement, and `cargo test` produces an unsigned binary every time.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use piebox::{ContainerStorage, Libkrun};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::OnceLock;

/// Prepared once per test process: the signed binary and a resolved rootfs.
struct Harness {
    binary: PathBuf,
    rootfs: PathBuf,
}

fn harness() -> Option<&'static Harness> {
    static HARNESS: OnceLock<Option<Harness>> = OnceLock::new();
    HARNESS.get_or_init(build_harness).as_ref()
}

/// Set this to turn every skip below into a failure (CI on a capable host).
const ENV_REQUIRE_GUEST: &str = "PIEBOX_REQUIRE_GUEST";

/// Skips, unless the host claims it should have been able to boot a guest.
fn skip(reason: &str) -> Option<Harness> {
    assert!(
        std::env::var_os(ENV_REQUIRE_GUEST).is_none(),
        "{ENV_REQUIRE_GUEST} is set but a guest could not be booted: {reason}"
    );
    eprintln!("skipping: {reason}");
    None
}

fn build_harness() -> Option<Harness> {
    if let Err(err) = Libkrun::load() {
        return skip(&format!("libkrun unavailable: {err}"));
    }

    let storage = ContainerStorage::from_env();
    let container = std::env::var("PIEBOX_CONTAINER")
        .unwrap_or_else(|_| "ubuntu-working-container".to_string());
    let rootfs = match piebox::container_rootfs(&storage, &container) {
        Ok(rootfs) => rootfs,
        Err(err) => return skip(&format!("no bootable rootfs: {err}")),
    };

    let binary = PathBuf::from(env!("CARGO_BIN_EXE_piebox"));
    if cfg!(target_os = "macos") {
        let entitlements = concat!(env!("CARGO_MANIFEST_DIR"), "/piebox.entitlements");
        let signed = Command::new("codesign")
            .args(["-s", "-", "-f", "--entitlements", entitlements])
            .arg(&binary)
            .output();
        match signed {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                return skip(&format!(
                    "could not sign the test binary: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(err) => return skip(&format!("codesign unavailable: {err}")),
        }
    }

    Some(Harness { binary, rootfs })
}

/// Runs a command in a guest. `args` precede `--`, `command` follows it.
fn run_in_guest(args: &[&str], command: &[&str]) -> Output {
    let harness = harness().expect("harness checked by the caller");
    Command::new(&harness.binary)
        .arg("run")
        .arg("--rootfs-path")
        .arg(&harness.rootfs)
        .args(args)
        .arg("--")
        .args(command)
        .output()
        .expect("spawn piebox")
}

/// Runs piebox against a real rootfs when there is one, else a plausible path.
///
/// Used for checks that must fail during validation, before any boot, so they
/// also run on hosts that cannot start a VM.
fn run_locally(args: &[&str], command: &[String]) -> Output {
    let rootfs = harness().map_or_else(|| PathBuf::from("/"), |h| h.rootfs.clone());
    Command::new(env!("CARGO_BIN_EXE_piebox"))
        .arg("run")
        .arg("--rootfs-path")
        .arg(rootfs)
        .args(args)
        .arg("--")
        .args(command)
        .output()
        .expect("spawn piebox")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Skips the test body when this host cannot boot a VM.
macro_rules! guest_or_skip {
    () => {
        if harness().is_none() {
            return;
        }
    };
}

#[test]
fn boots_a_guest_running_a_modern_linux_kernel() {
    guest_or_skip!();
    let output = run_in_guest(&[], &["/usr/bin/uname", "-r"]);
    let release = stdout_of(&output);
    // Asserting the exact libkrunfw kernel would break on every upgrade; what
    // matters is that a Linux kernel new enough to be useful actually booted.
    let major: u32 = release
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or_else(|| panic!("not a kernel version: {release:?}"));
    assert!(major >= 6, "guest kernel too old: {release}");
}

#[test]
fn propagates_the_workload_exit_code() {
    guest_or_skip!();
    let output = run_in_guest(&[], &["/bin/sh", "-c", "exit 42"]);
    assert_eq!(output.status.code(), Some(42));
}

#[test]
fn a_successful_workload_exits_zero() {
    guest_or_skip!();
    let output = run_in_guest(&[], &["/bin/true"]);
    assert!(output.status.success(), "{output:?}");
}

/// The point of a VM: the host's environment must not appear inside it.
#[test]
fn host_environment_does_not_leak_into_the_guest() {
    guest_or_skip!();
    let harness = harness().expect("checked");
    let output = Command::new(&harness.binary)
        .arg("run")
        .arg("--rootfs-path")
        .arg(&harness.rootfs)
        .env("PIEBOX_TEST_SECRET", "leaked")
        .args(["--", "/bin/sh", "-c", "echo secret=[$PIEBOX_TEST_SECRET]"])
        .output()
        .expect("spawn piebox");
    assert_eq!(stdout_of(&output), "secret=[]");
}

#[test]
fn explicit_environment_reaches_the_workload() {
    guest_or_skip!();
    let output = run_in_guest(&["-e", "FOO=bar"], &["/bin/sh", "-c", "echo FOO=$FOO"]);
    assert_eq!(stdout_of(&output), "FOO=bar");
}

#[test]
fn workdir_is_applied() {
    guest_or_skip!();
    let output = run_in_guest(&["--workdir", "/tmp"], &["/bin/pwd"]);
    assert_eq!(stdout_of(&output), "/tmp");
}

#[test]
fn requested_sizing_reaches_the_guest() {
    guest_or_skip!();
    let output = run_in_guest(&["--cpus", "1"], &["/usr/bin/nproc"]);
    assert_eq!(stdout_of(&output), "1");
}

/// libkrun's init reuses 127 for "cannot find the executable", so the failure
/// has to be named rather than passed through as a mysterious status.
#[test]
fn a_missing_guest_command_is_reported() {
    guest_or_skip!();
    let output = run_in_guest(&[], &["/usr/bin/definitely-not-installed"]);
    assert_eq!(output.status.code(), Some(127));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found"), "{stderr}");
}

/// The other half of the above: a workload's own 127 is indistinguishable, and
/// that ambiguity is intended rather than a bug to be "fixed" later.
#[test]
fn a_workload_exiting_127_looks_the_same() {
    guest_or_skip!();
    let output = run_in_guest(&[], &["/bin/sh", "-c", "exit 127"]);
    assert_eq!(output.status.code(), Some(127));
}

// Everything below guards a way the guest kernel command line fails silently:
// libkrun appends arguments and environment to it, quoting each token without
// escaping anything. Each of these used to "succeed" while running nothing, or
// while running something else entirely.

#[test]
fn too_many_arguments_are_refused_instead_of_running_nothing() {
    let args: Vec<String> = (0..40).map(|i| i.to_string()).collect();
    let mut command = vec!["/bin/echo".to_string()];
    command.extend(args);
    let output = run_locally(&[], &command);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("at most"), "{stderr}");
}

#[test]
fn a_quote_in_an_argument_is_refused() {
    let output = run_locally(&[], &["/bin/echo".into(), "a\"b".into()]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot encode"),
        "{output:?}"
    );
}

/// This one could replace the workload outright: everything before `--` on the
/// command line is read as kernel parameters, so `" init=/bin/echo "` in an
/// environment value used to make the real command never run.
#[test]
fn an_environment_value_cannot_inject_kernel_parameters() {
    let output = run_locally(
        &["-e", "A=1\" init=/bin/echo \""],
        &["/bin/false".to_string()],
    );
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot encode"),
        "{output:?}"
    );
}

#[test]
fn an_over_long_command_line_is_refused_instead_of_aborting() {
    let output = run_locally(&[], &["/bin/echo".to_string(), "x".repeat(4096)]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("command line"), "{stderr}");
    // An abort from inside libkrun would show up as a signal death.
    assert!(output.status.code().is_some(), "{output:?}");
}

/// Not a transport problem, and not fixable by one: Linux's `parse_args` stops
/// at a `--`, and the kernel parses the part after libkrun's own `--` with that
/// same function, so a second one truncates init's argv. Measured: passing
/// `a -- b` to /bin/echo printed "a". Refused loudly instead.
#[test]
fn a_literal_double_dash_argument_is_refused_not_dropped() {
    let output = run_locally(
        &[],
        &["/bin/echo".to_string(), "--".to_string(), "b".to_string()],
    );
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("truncate"),
        "{output:?}"
    );
}

#[test]
fn a_nonexistent_workdir_is_refused_rather_than_ignored() {
    guest_or_skip!();
    let output = run_in_guest(&["--workdir", "/no/such/directory"], &["/bin/pwd"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not exist in the guest"),
        "{output:?}"
    );
}

/// Parent-side validation only, so this runs even where no guest can boot.
#[test]
fn an_unusable_rootfs_is_refused_before_booting() {
    let output = Command::new(env!("CARGO_BIN_EXE_piebox"))
        .arg("run")
        .args(["--rootfs-path", "/nonexistent/piebox-rootfs"])
        .args(["--", "/bin/true"])
        .output()
        .expect("spawn piebox");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rootfs"), "{stderr}");
}

// --- The parent/child boundary -------------------------------------------
//
// The spec travels on a pipe rather than as command-line flags. These check
// the two things that buys: nothing about the guest is visible in the VMM
// child's argv, and the VM does not outlive the process that asked for it.

/// PID of the VMM process spawned by `parent`, once it exists.
fn vmm_child_of(parent: u32) -> Option<u32> {
    for _ in 0..100 {
        let output = Command::new("pgrep")
            .args(["-P", &parent.to_string()])
            .output()
            .expect("pgrep");
        if let Some(pid) = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .and_then(|pid| pid.parse().ok())
        {
            return Some(pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

fn is_alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn argv_of(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// argv is world-readable, so the guest's command and environment must not be
/// there. Passed as flags, `-e TOKEN=…` used to be visible to every user.
#[test]
fn the_vmm_child_reveals_nothing_about_the_guest_in_its_argv() {
    guest_or_skip!();
    let harness = harness().expect("checked");
    let mut parent = Command::new(&harness.binary)
        .arg("run")
        .arg("--rootfs-path")
        .arg(&harness.rootfs)
        .args(["-e", "TOKEN=sup3rs3cret"])
        .args(["--", "/bin/sleep", "10"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn piebox");

    let vmm = vmm_child_of(parent.id()).expect("VMM child never appeared");
    let argv = argv_of(vmm);
    let _ = parent.kill();
    let _ = parent.wait();

    assert!(
        !argv.contains("sup3rs3cret"),
        "secret leaked into argv: {argv}"
    );
    assert!(
        !argv.contains("/bin/sleep"),
        "guest command in argv: {argv}"
    );
    assert!(argv.contains("__vmm"), "unexpected VMM argv: {argv}");
}

/// A secret passed by name is never written on any command line at all.
#[test]
fn an_environment_variable_can_be_passed_by_name_instead_of_value() {
    guest_or_skip!();
    let harness = harness().expect("checked");
    let output = Command::new(&harness.binary)
        .arg("run")
        .arg("--rootfs-path")
        .arg(&harness.rootfs)
        .args(["-e", "TOKEN"])
        .env("TOKEN", "sup3rs3cret")
        .args(["--", "/bin/sh", "-c", "echo got=$TOKEN"])
        .output()
        .expect("spawn piebox");
    assert_eq!(stdout_of(&output), "got=sup3rs3cret");
}

/// A bare name that is not set must be an error rather than an empty value,
/// which would look like the variable had been passed.
#[test]
fn an_unset_environment_name_is_an_error() {
    let output = run_locally(
        &["-e", "PIEBOX_DEFINITELY_NOT_SET"],
        &["/bin/true".to_string()],
    );
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not set"),
        "{output:?}"
    );
}

/// Killing the parent used to leave a microVM running with its RAM and vCPU
/// threads, reparented and unfindable. The spec pipe doubles as a lifeline:
/// its EOF tells the child nobody is waiting for it any more.
#[test]
fn killing_the_parent_shuts_the_microvm_down() {
    guest_or_skip!();
    let harness = harness().expect("checked");
    let mut parent = Command::new(&harness.binary)
        .arg("run")
        .arg("--rootfs-path")
        .arg(&harness.rootfs)
        .args(["--", "/bin/sleep", "120"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn piebox");

    let vmm = vmm_child_of(parent.id()).expect("VMM child never appeared");
    assert!(is_alive(vmm), "VMM should be running");

    // SIGKILL: the parent gets no chance to clean up after itself.
    parent.kill().expect("kill parent");
    parent.wait().expect("reap parent");

    for _ in 0..100 {
        if !is_alive(vmm) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // Do not leave a stray VM behind for the next test run.
    let _ = Command::new("kill").args(["-9", &vmm.to_string()]).status();
    panic!("VMM {vmm} outlived its parent");
}
