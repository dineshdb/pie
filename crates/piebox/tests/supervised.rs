//! Running commands through the in-guest supervisor.
//!
//! The point of this path is that nothing travels on the guest's kernel command
//! line, so every limit of the boot path disappears. Each test below is a case
//! that boot mode either refuses or gets wrong.
//!
//! Needs a hypervisor and a cross-compiled supervisor (`just piebox-guest`), so
//! it skips when either is missing — unless `PIEBOX_REQUIRE_GUEST` is set.

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

const ENV_REQUIRE_GUEST: &str = "PIEBOX_REQUIRE_GUEST";

struct Harness {
    binary: PathBuf,
    rootfs: PathBuf,
}

fn harness() -> Option<&'static Harness> {
    static HARNESS: OnceLock<Option<Harness>> = OnceLock::new();
    HARNESS.get_or_init(build_harness).as_ref()
}

fn skip(reason: &str) -> Option<Harness> {
    assert!(
        std::env::var_os(ENV_REQUIRE_GUEST).is_none(),
        "{ENV_REQUIRE_GUEST} is set but a supervised guest could not run: {reason}"
    );
    eprintln!("skipping: {reason}");
    None
}

fn build_harness() -> Option<Harness> {
    if let Err(err) = Libkrun::load() {
        return skip(&format!("libkrun unavailable: {err}"));
    }
    // The supervisor is built for a different target than this test, so it has
    // to exist already.
    if let Err(err) = piebox::guest_binary() {
        return skip(&format!("{err}"));
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
        match Command::new("codesign")
            .args(["-s", "-", "-f", "--entitlements", entitlements])
            .arg(&binary)
            .output()
        {
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

macro_rules! guest_or_skip {
    () => {
        if harness().is_none() {
            return;
        }
    };
}

fn supervised(args: &[&str], command: &[&str]) -> Output {
    let harness = harness().expect("checked by the caller");
    Command::new(&harness.binary)
        .arg("run")
        .arg("--rootfs-path")
        .arg(&harness.rootfs)
        .arg("--supervised")
        .args(args)
        .arg("--")
        .args(command)
        .output()
        .expect("spawn piebox")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Asserts on stdout, reporting stderr and the exit status when it differs.
///
/// Without stderr in the message a failure here says only "expected X, got
/// empty", while the reason is always in stderr — piebox puts the guest's
/// console tail there.
fn assert_stdout(output: &Output, expected: &str) {
    assert_eq!(
        stdout_of(output),
        expected,
        "\n  exit: {:?}\n  stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_command_containing_quotes_runs() {
    guest_or_skip!();
    // Boot mode refuses this: libkrun quotes command-line tokens without
    // escaping, so a `"` would break out into kernel parameters.
    let output = supervised(&[], &["/bin/sh", "-c", "echo \"hello world\""]);
    assert_stdout(&output, "hello world");
}

#[test]
fn a_literal_double_dash_reaches_the_command() {
    guest_or_skip!();
    // Boot mode refuses this: the guest kernel stops parsing init arguments
    // at a `--`.
    let output = supervised(&[], &["/bin/echo", "a", "--", "b"]);
    assert_stdout(&output, "a -- b");
}

#[test]
fn far_more_arguments_than_a_command_line_allows() {
    guest_or_skip!();
    let args: Vec<String> = (1..=60).map(|i| i.to_string()).collect();
    let mut command = vec!["/bin/echo".to_string()];
    command.extend(args.clone());
    let borrowed: Vec<&str> = command.iter().map(String::as_str).collect();

    let output = supervised(&[], &borrowed);
    assert_eq!(stdout_of(&output), args.join(" "));
}

#[test]
fn an_argument_far_longer_than_a_command_line_allows() {
    guest_or_skip!();
    let big = "x".repeat(8192);
    let output = supervised(
        &[],
        &["/bin/sh", "-c", "printf %s \"$1\" | wc -c", "sh", &big],
    );
    assert_stdout(&output, "8192");
}

#[test]
fn the_exit_code_comes_back() {
    guest_or_skip!();
    assert_eq!(supervised(&[], &["/bin/true"]).status.code(), Some(0));
    assert_eq!(
        supervised(&[], &["/bin/sh", "-c", "exit 42"]).status.code(),
        Some(42)
    );
}

/// Boot mode multiplexes everything onto the console; here they stay apart.
#[test]
fn stdout_and_stderr_stay_separate() {
    guest_or_skip!();
    let output = supervised(&[], &["/bin/sh", "-c", "echo OUT; echo ERR >&2"]);
    assert_stdout(&output, "OUT");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("ERR"),
        "{output:?}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("OUT"),
        "stdout leaked into stderr: {output:?}"
    );
}

/// Output frames carry bytes, not text, so binary output must survive intact.
#[test]
fn output_that_is_not_text_survives() {
    guest_or_skip!();
    let output = supervised(&[], &["/bin/sh", "-c", "printf '\\000\\377\\376'"]);
    assert_eq!(output.stdout, vec![0x00, 0xff, 0xfe]);
}

#[test]
fn a_large_amount_of_output_is_not_truncated() {
    guest_or_skip!();
    let output = supervised(&[], &["/bin/sh", "-c", "seq 1 200000"]);
    let lines = output.stdout.iter().filter(|b| **b == b'\n').count();
    assert_eq!(lines, 200_000, "expected every line back");
}

#[test]
fn the_environment_is_exactly_what_was_asked_for() {
    guest_or_skip!();
    let harness = harness().expect("checked");
    let output = Command::new(&harness.binary)
        .arg("run")
        .arg("--rootfs-path")
        .arg(&harness.rootfs)
        .arg("--supervised")
        .args(["-e", "TOKEN"])
        .env("TOKEN", "sup3rs3cret")
        .env("PIEBOX_TEST_LEAK", "leaked")
        .args([
            "--",
            "/bin/sh",
            "-c",
            "echo got=$TOKEN leak=[$PIEBOX_TEST_LEAK]",
        ])
        .output()
        .expect("spawn piebox");
    assert_stdout(&output, "got=sup3rs3cret leak=[]");
}

#[test]
fn the_working_directory_is_applied() {
    guest_or_skip!();
    let output = supervised(&["--workdir", "/tmp"], &["/bin/pwd"]);
    assert_stdout(&output, "/tmp");
}

/// A shell reports 127 for this, and so should the supervisor -- with an
/// explanation, since the guest is the only place that knows what happened.
#[test]
fn a_missing_command_reports_127_and_says_why() {
    guest_or_skip!();
    let output = supervised(&[], &["/bin/definitely-not-installed"]);
    assert_eq!(output.status.code(), Some(127));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not run"),
        "{output:?}"
    );
}

// --- Protocol-level guards ------------------------------------------------
//
// These stand in for the guest with a plain unix socket, so they need no
// hypervisor and run everywhere. They cover the failure modes a real guest
// only reaches when something has already gone wrong.

use piebox::Endpoint;
use std::io::Write;
use std::time::Duration;

/// A `Failure` with no `Exit` used to hang the host forever: it prints the
/// diagnostic and goes back to reading. Every request must be answered by
/// exactly one `Exit`, and the read deadline is the backstop when it is not.
#[test]
fn a_failure_without_an_exit_does_not_hang_the_host() {
    let endpoint = Endpoint::bind().expect("bind");
    let socket = endpoint.socket().to_path_buf();

    let guest = std::thread::spawn(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
        // Read the request, then answer with a bare Failure and stay silent.
        let mut header = [0u8; 5];
        std::io::Read::read_exact(&mut stream, &mut header).expect("header");
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut body = vec![0u8; len];
        std::io::Read::read_exact(&mut stream, &mut body).expect("body");

        let message = b"nope";
        stream.write_all(&[5]).expect("kind");
        stream
            .write_all(&u32::try_from(message.len()).expect("len").to_be_bytes())
            .expect("len");
        stream.write_all(message).expect("payload");
        stream.flush().ok();
        // Hold the connection open, saying nothing more, for longer than the
        // host's deadline below.
        std::thread::sleep(Duration::from_millis(1500));
    });

    let mut supervisor = endpoint.accept(Duration::from_secs(5)).expect("accept");
    let request = piebox::Request {
        program: "/bin/true".to_string(),
        args: Vec::new(),
        env: Vec::new(),
        cwd: None,
        mounts: Vec::new(),
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result =
        supervisor.run_with_timeout(&request, &mut out, &mut err, Duration::from_millis(300));

    let err_text = result
        .expect_err("must give up rather than wait forever")
        .to_string();
    assert!(err_text.contains("sent nothing"), "{err_text}");
    // The diagnostic still reached the caller.
    assert!(String::from_utf8_lossy(&err).contains("nope"), "{err:?}");
    drop(supervisor);
    let _ = guest.join();
}

/// One connection is meant to serve many commands, which is the foundation for
/// a persistent VM. Nothing exercised that until now.
#[test]
fn one_connection_serves_several_commands() {
    let endpoint = Endpoint::bind().expect("bind");
    let socket = endpoint.socket().to_path_buf();

    let guest = std::thread::spawn(move || {
        let mut stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
        for reply in ["first", "second", "third"] {
            let mut header = [0u8; 5];
            if std::io::Read::read_exact(&mut stream, &mut header).is_err() {
                return;
            }
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut body = vec![0u8; len];
            std::io::Read::read_exact(&mut stream, &mut body).expect("body");

            // stdout frame, then the exit status.
            stream.write_all(&[2]).expect("kind");
            stream
                .write_all(&u32::try_from(reply.len()).expect("len").to_be_bytes())
                .expect("len");
            stream.write_all(reply.as_bytes()).expect("payload");

            let exit = br#"{"code":0,"signal":null}"#;
            stream.write_all(&[4]).expect("kind");
            stream
                .write_all(&u32::try_from(exit.len()).expect("len").to_be_bytes())
                .expect("len");
            stream.write_all(exit).expect("payload");
            stream.flush().ok();
        }
    });

    let mut supervisor = endpoint.accept(Duration::from_secs(5)).expect("accept");
    let request = piebox::Request {
        program: "/bin/true".to_string(),
        args: Vec::new(),
        env: Vec::new(),
        cwd: None,
        mounts: Vec::new(),
    };
    for expected in ["first", "second", "third"] {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let exit = supervisor
            .run_with_timeout(&request, &mut out, &mut err, Duration::from_secs(5))
            .expect("each command should complete");
        assert_eq!(i32::from(exit), 0);
        assert_eq!(String::from_utf8_lossy(&out), expected);
    }
    drop(supervisor);
    let _ = guest.join();
}

/// A supervisor already sitting in the rootfs is never reused: the guest can
/// write there over virtio-fs, and this binary receives every command and every
/// secret. Each run also needs its own file, because replacing a shared one
/// makes a concurrently booting guest fail to exec it.
#[test]
fn every_run_stages_its_own_supervisor_and_reuses_none() {
    let temp = std::env::temp_dir().join(format!("piebox-restage-{}", std::process::id()));
    let rootfs = temp.join("rootfs");
    std::fs::create_dir_all(rootfs.join(piebox::GUEST_STAGING_DIR)).expect("rootfs");
    let binary = temp.join("piebox-guest");
    std::fs::write(&binary, b"REAL-SUPERVISOR").expect("write");

    // A tampered leftover, same length as the real thing.
    let planted = rootfs
        .join(piebox::GUEST_STAGING_DIR)
        .join(format!("{}planted", piebox::GUEST_BINARY_PREFIX));
    std::fs::write(&planted, b"EVIL-SUPERVISOR").expect("plant");

    let first = piebox::stage_guest_binary(&rootfs, &binary).expect("stage");
    let second = piebox::stage_guest_binary(&rootfs, &binary).expect("stage again");
    assert_ne!(
        first.guest_path(),
        second.guest_path(),
        "two runs must not share a staged path"
    );

    for staged in [&first, &second] {
        let host_path = rootfs.join(
            staged
                .guest_path()
                .strip_prefix("/")
                .expect("guest path is absolute"),
        );
        assert_eq!(
            std::fs::read(&host_path).expect("read staged"),
            b"REAL-SUPERVISOR",
            "a staged supervisor must be the one piebox shipped"
        );
        // Nothing planted is ever what gets executed.
        assert_ne!(host_path, planted);
    }

    // Dropping the guard takes the file back out of the image.
    let host_path = rootfs.join(first.guest_path().strip_prefix("/").expect("absolute"));
    drop(first);
    assert!(
        !host_path.exists(),
        "a staged supervisor must not be left behind"
    );

    std::fs::remove_dir_all(&temp).ok();
}

/// libkrun sets O_NONBLOCK on the descriptors it uses for the guest console.
/// That flag lives on the shared open file description, so leaving the caller's
/// own stdin/stderr attached would corrupt them for the caller too.
#[test]
fn piebox_does_not_leave_the_callers_pipes_non_blocking() {
    guest_or_skip!();
    let harness = harness().expect("checked");

    let (stdin_read, _stdin_write) = std::io::pipe().expect("pipe");
    let (stderr_read, stderr_write) = std::io::pipe().expect("pipe");
    let nonblocking = |fd: std::os::fd::RawFd| -> bool {
        // SAFETY: F_GETFL only inspects the descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        flags != -1 && flags & libc::O_NONBLOCK != 0
    };
    use std::os::fd::AsRawFd;
    assert!(!nonblocking(stdin_read.as_raw_fd()));
    assert!(!nonblocking(stderr_read.as_raw_fd()));

    let status = Command::new(&harness.binary)
        .arg("run")
        .arg("--rootfs-path")
        .arg(&harness.rootfs)
        .arg("--supervised")
        .args(["--", "/bin/true"])
        .stdin(std::process::Stdio::from(
            stdin_read.try_clone().expect("clone"),
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_write))
        .status()
        .expect("spawn piebox");
    assert!(status.success());

    assert!(
        !nonblocking(stdin_read.as_raw_fd()),
        "piebox left the caller's stdin non-blocking"
    );
    assert!(
        !nonblocking(stderr_read.as_raw_fd()),
        "piebox left the caller's stderr non-blocking"
    );
}

/// A bad working directory must name the directory, not blame the program.
#[test]
fn a_nonexistent_workdir_is_named() {
    guest_or_skip!();
    let output = supervised(&["--workdir", "/no/such/directory"], &["/bin/pwd"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("/no/such/directory"), "{stderr}");
    assert!(
        !stderr.contains("could not run /bin/pwd"),
        "a bad workdir must not read as a missing program: {stderr}"
    );
}

// --- Mounts ---------------------------------------------------------------
//
// piebox's own mounts: a host directory becomes a virtio-fs device, and the
// guest supervisor mounts it. Only the supervised path can do this, because
// libkrun's host-side mounting API is no longer supported.

/// A scratch host directory, removed when the test ends.
struct Workspace(PathBuf);

impl Workspace {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("piebox-work-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&path).expect("workspace");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

#[test]
fn a_mounted_host_directory_is_readable_in_the_guest() {
    guest_or_skip!();
    let work = Workspace::new("read");
    std::fs::write(work.path().join("from-host.txt"), b"host wrote this\n").expect("write");

    let spec = format!("{}:/work", work.path().display());
    let output = supervised(&["--volume", &spec], &["/bin/cat", "/work/from-host.txt"]);
    assert_stdout(&output, "host wrote this");
}

#[test]
fn what_the_guest_writes_appears_on_the_host() {
    guest_or_skip!();
    let work = Workspace::new("write");
    let spec = format!("{}:/work", work.path().display());

    let output = supervised(
        &["--volume", &spec],
        &[
            "/bin/sh",
            "-c",
            "echo guest wrote this > /work/from-guest.txt",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let written = std::fs::read_to_string(work.path().join("from-guest.txt")).expect("host read");
    assert_eq!(written.trim(), "guest wrote this");
}

/// Read-only is enforced twice over: the host exposes the device read-only and
/// the guest mounts it MS_RDONLY, so neither side alone failing lets a write
/// through.
#[test]
fn a_read_only_mount_cannot_be_written_to() {
    guest_or_skip!();
    let work = Workspace::new("ro");
    std::fs::write(work.path().join("readable.txt"), b"still readable\n").expect("write");
    let spec = format!("{}:/work:ro", work.path().display());

    let output = supervised(
        &["--volume", &spec],
        &["/bin/sh", "-c", "echo nope > /work/blocked.txt"],
    );
    assert!(!output.status.success(), "a write must fail: {output:?}");
    assert!(
        !work.path().join("blocked.txt").exists(),
        "nothing may reach the host"
    );

    // ...and reading still works.
    let output = supervised(&["--volume", &spec], &["/bin/cat", "/work/readable.txt"]);
    assert_stdout(&output, "still readable");
}

#[test]
fn several_directories_can_be_mounted_at_once() {
    guest_or_skip!();
    let one = Workspace::new("multi-one");
    let two = Workspace::new("multi-two");
    std::fs::write(one.path().join("a"), b"first\n").expect("write");
    std::fs::write(two.path().join("b"), b"second\n").expect("write");

    let output = supervised(
        &[
            "--volume",
            &format!("{}:/one", one.path().display()),
            "--volume",
            &format!("{}:/two:ro", two.path().display()),
        ],
        &["/bin/sh", "-c", "cat /one/a /two/b"],
    );
    assert_stdout(&output, "first\nsecond");
}

/// The mount point need not exist in the image: piebox creates it.
#[test]
fn a_mount_point_that_does_not_exist_yet_is_created() {
    guest_or_skip!();
    let work = Workspace::new("mkdir");
    std::fs::write(work.path().join("f"), b"deep\n").expect("write");
    let spec = format!("{}:/piebox-created/deeper", work.path().display());

    let output = supervised(
        &["--volume", &spec],
        &["/bin/cat", "/piebox-created/deeper/f"],
    );
    assert_stdout(&output, "deep");
}

/// Asking for a mount selects the supervised path, since only the guest can
/// mount one — the caller should not have to know that.
#[test]
fn a_mount_does_not_require_passing_supervised() {
    guest_or_skip!();
    let harness = harness().expect("checked");
    let work = Workspace::new("implies");
    std::fs::write(work.path().join("f"), b"implied\n").expect("write");

    let output = Command::new(&harness.binary)
        .arg("run")
        .arg("--rootfs-path")
        .arg(&harness.rootfs)
        .args(["--volume", &format!("{}:/work", work.path().display())])
        .args(["--", "/bin/cat", "/work/f"])
        .output()
        .expect("spawn piebox");
    assert_stdout(&output, "implied");
}

/// Runs piebox without needing a bootable guest: a bad mount is rejected during
/// validation, before anything is started.
fn run_expecting_rejection(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_piebox"))
        .arg("run")
        .args(["--rootfs-path", "/"])
        .args(args)
        .args(["--", "/bin/true"])
        .output()
        .expect("spawn piebox")
}

#[test]
fn a_bad_mount_specification_is_refused_before_booting() {
    let output = run_expecting_rejection(&["--volume", "/nonexistent/piebox-src:/work"]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("mount"), "{stderr}");

    for bad in [
        "missing-colon",
        "/tmp:relative-guest-path",
        "/tmp:/work:maybe",
    ] {
        let output = run_expecting_rejection(&["--volume", bad]);
        assert!(
            !output.status.success(),
            "{bad:?} must be refused: {output:?}"
        );
    }
}

/// `:ro` has to hold against a guest that actively fights it. The guest *can*
/// flip its own mount table to `rw` — only the host-side device flag stops the
/// write, which is the half a compromised guest cannot reach.
#[test]
fn a_read_only_mount_survives_the_guest_remounting_it_rw() {
    guest_or_skip!();
    let work = Workspace::new("remount");
    let spec = format!("{}:/ro:ro", work.path().display());

    let output = supervised(
        &["--volume", &spec],
        &[
            "/bin/sh",
            "-c",
            "mount -o remount,rw /ro; echo escaped > /ro/escaped.txt",
        ],
    );
    assert!(!output.status.success(), "the write must fail: {output:?}");
    assert!(
        !work.path().join("escaped.txt").exists(),
        "a remount must not let a write reach the host"
    );
}

/// A symlink inside a mounted directory must resolve in the *guest*, not on the
/// host. virtio-fs sends LOOKUP per component and hands symlinks back to the
/// guest kernel, so this holds — worth pinning so it cannot regress quietly.
#[test]
fn symlinks_in_a_mount_cannot_reach_host_files() {
    guest_or_skip!();
    let work = Workspace::new("symlink");
    std::os::unix::fs::symlink("/etc/hostname", work.path().join("absolute")).expect("symlink");
    std::os::unix::fs::symlink("../../../../etc/hostname", work.path().join("relative"))
        .expect("symlink");

    // The host's /etc/hostname either does not exist or is not the guest's.
    let host_content = std::fs::read_to_string("/etc/hostname").unwrap_or_default();

    for link in ["absolute", "relative"] {
        let output = supervised(
            &["--volume", &format!("{}:/work", work.path().display())],
            &["/bin/cat", &format!("/work/{link}")],
        );
        let seen = stdout_of(&output);
        assert!(
            seen.is_empty() || seen != host_content.trim(),
            "{link} resolved to the host's file: {seen:?}"
        );
    }
}

/// Mounting over a path the guest already has mounted would either hide
/// something the guest needs or silently run against the wrong filesystem.
#[test]
fn a_target_that_is_already_a_mount_point_is_refused() {
    guest_or_skip!();
    let work = Workspace::new("occupied");
    std::fs::write(work.path().join("f"), b"mine\n").expect("write");

    // /dev/shm is a tmpfs mounted by libkrun's init before the supervisor runs.
    let output = supervised(
        &["--volume", &format!("{}:/dev/shm", work.path().display())],
        &["/bin/cat", "/dev/shm/f"],
    );
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already mounted"), "{stderr}");
}

/// The whole point of a mount for pie: run a command *in* the mounted checkout.
/// The directory does not exist in the image, so a host-side existence check
/// must not refuse it.
#[test]
fn the_working_directory_can_be_inside_a_mount() {
    guest_or_skip!();
    let work = Workspace::new("cwd");
    std::fs::write(work.path().join("marker"), b"here\n").expect("write");
    let target = format!("/piebox-cwd-{}", std::process::id());

    let output = supervised(
        &[
            "--volume",
            &format!("{}:{target}", work.path().display()),
            "--workdir",
            &target,
        ],
        &["/bin/sh", "-c", "pwd; cat marker"],
    );
    assert_stdout(&output, &format!("{target}\nhere"));
}

#[test]
fn nested_and_reserved_targets_are_refused_without_booting() {
    let work = Workspace::new("refused");
    let host = work.path().display().to_string();
    let cases: Vec<Vec<String>> = vec![
        // nested
        vec![
            "--volume".into(),
            format!("{host}:/n"),
            "--volume".into(),
            format!("{host}:/n/inner"),
        ],
        // reserved for piebox itself
        vec!["--volume".into(), format!("{host}:/.piebox")],
        // traversal: `/bar/..` is `/`
        vec!["--volume".into(), format!("{host}:/bar/..")],
        // the same target twice
        vec![
            "--volume".into(),
            format!("{host}:/d"),
            "--volume".into(),
            format!("{host}:/d"),
        ],
    ];
    for case in &cases {
        let args: Vec<&str> = case.iter().map(String::as_str).collect();
        let output = run_expecting_rejection(&args);
        assert!(!output.status.success(), "{args:?} must be refused");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("mount"),
            "{args:?}: {output:?}"
        );
    }
}
