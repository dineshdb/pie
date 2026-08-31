//! Host side of the guest supervisor: the socket, and the client that talks to it.
//!
//! Running a command this way costs one extra hop but removes every limit of
//! the boot path, because nothing goes near the guest's kernel command line:
//! quotes, arbitrary length, any number of arguments and a literal `--` all
//! arrive intact, and stdout and stderr stay separate.

use crate::error::{Error, Result};
use piebox_proto::{Exit, Kind, Request, read_frame, write_json};
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Directory inside the guest rootfs where supervisors are staged.
pub const GUEST_STAGING_DIR: &str = ".piebox";

/// Prefix of a staged supervisor. Each run stages its own file under this.
pub const GUEST_BINARY_PREFIX: &str = "piebox-guest-";

/// Staged supervisors older than this are assumed to be from a crashed run.
const STALE_AFTER: Duration = Duration::from_secs(3600);

/// Overrides where the host looks for the cross-compiled supervisor.
pub const ENV_GUEST_BIN: &str = "PIEBOX_GUEST_BIN";

/// How long the host waits between frames before declaring the guest wedged.
///
/// Generous, because a command may legitimately think for a long time before
/// writing anything; it exists only so a stuck supervisor cannot hang piebox.
pub const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(600);

/// `sun_path` is 104 bytes on macOS, and a socket whose path does not fit fails
/// in confusing ways, so the limit is checked rather than discovered.
const MAX_SOCKET_PATH: usize = 100;

/// A listening socket for one guest to connect back to.
///
/// Lives in a directory only this user can enter: a socket anyone could reach
/// would let any local process run commands inside the guest.
#[derive(Debug)]
pub struct Endpoint {
    listener: UnixListener,
    socket: PathBuf,
    directory: PathBuf,
}

impl Endpoint {
    /// Creates the private directory and binds the socket.
    ///
    /// # Errors
    /// Fails if the directory or socket cannot be created, or the resulting
    /// path is too long for a unix socket.
    pub fn bind() -> Result<Self> {
        // Unique per endpoint, not per process: two VMs driven from one process
        // must not share a directory, or the first one dropped takes the
        // other's socket with it.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Unguessable, so nobody can pre-plant the directory (or a symlink
        // where it will be) and end up owning the control socket.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let directory = std::env::temp_dir().join(format!(
            "piebox-{}-{serial}-{nonce:08x}",
            std::process::id()
        ));
        // Created 0700 in one step, failing if it exists: create-then-chmod is
        // world-traversable in between, and follows a symlink planted there.
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|err| Error::Command {
                program: "piebox",
                detail: format!("could not create {}: {err}", directory.display()),
            })?;

        // Deliberately short: the directory already carries the identity.
        let socket = directory.join("s");
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(Error::invalid(
                "socket path",
                format!(
                    "{} is {} bytes, above the {MAX_SOCKET_PATH}-byte limit for a unix socket",
                    socket.display(),
                    socket.as_os_str().len()
                ),
            ));
        }
        // A leftover socket from a crashed run would make bind fail.
        let _ = std::fs::remove_file(&socket);

        let listener = UnixListener::bind(&socket).map_err(|err| Error::Command {
            program: "piebox",
            detail: format!("could not bind {}: {err}", socket.display()),
        })?;

        Ok(Self {
            listener,
            socket,
            directory,
        })
    }

    /// Path the guest's vsock traffic is forwarded to.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Waits for the guest supervisor to connect.
    ///
    /// # Errors
    /// Fails if the guest does not connect within `timeout`, which usually
    /// means the supervisor is not in the rootfs or could not start.
    pub fn accept(&self, timeout: Duration) -> Result<Supervisor> {
        self.listener.set_nonblocking(true).ok();
        let deadline = Instant::now() + timeout;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .map_err(|err| Error::Command {
                            program: "piebox",
                            detail: format!("could not configure the control socket: {err}"),
                        })?;
                    return Supervisor::new(stream);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(Error::Command {
                            program: "piebox",
                            detail: format!(
                                "the guest supervisor did not connect within {timeout:?}; the guest console output above usually says why"
                            ),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(err) => {
                    return Err(Error::Command {
                        program: "piebox",
                        detail: format!("could not accept the guest connection: {err}"),
                    });
                }
            }
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        // Whole tree: callers put the console log in here too, and remove_dir
        // would leave the directory behind on every failed run.
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// A connected guest supervisor.
#[derive(Debug)]
pub struct Supervisor {
    reader: std::io::BufReader<UnixStream>,
    writer: UnixStream,
}

impl Supervisor {
    fn new(stream: UnixStream) -> Result<Self> {
        let reader = stream.try_clone().map_err(|err| Error::Command {
            program: "piebox",
            detail: format!("could not split the control connection: {err}"),
        })?;
        Ok(Self {
            reader: std::io::BufReader::new(reader),
            writer: stream,
        })
    }

    /// Runs one command in the guest, streaming its output to `out` and `err`.
    ///
    /// # Errors
    /// Fails if the connection breaks, or the supervisor reports it could not
    /// run the command at all.
    pub fn run(
        &mut self,
        request: &Request,
        out: &mut impl Write,
        err: &mut impl Write,
    ) -> Result<Exit> {
        self.run_with_timeout(request, out, err, DEFAULT_REPLY_TIMEOUT)
    }

    /// [`Supervisor::run`] with an explicit deadline between frames.
    ///
    /// # Errors
    /// Fails if the guest goes quiet for `timeout`. That deadline is the only
    /// thing standing between a wedged supervisor and a piebox that never
    /// returns.
    pub fn run_with_timeout(
        &mut self,
        request: &Request,
        out: &mut impl Write,
        err: &mut impl Write,
        timeout: Duration,
    ) -> Result<Exit> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .map_err(|source| Error::Command {
                program: "piebox",
                detail: format!("could not set a read timeout: {source}"),
            })?;

        write_json(&mut self.writer, Kind::Request, request).map_err(|source| Error::Command {
            program: "piebox",
            detail: format!("could not send the command to the guest: {source}"),
        })?;

        loop {
            let frame = read_frame(&mut self.reader).map_err(|source| {
                if matches!(
                    source.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) {
                    return Error::Command {
                        program: "piebox",
                        detail: format!(
                            "the guest supervisor sent nothing for {timeout:?}; giving up"
                        ),
                    };
                }
                Error::Command {
                    program: "piebox",
                    detail: format!("bad frame from the guest supervisor: {source}"),
                }
            })?;
            let Some((kind, payload)) = frame else {
                return Err(Error::Command {
                    program: "piebox",
                    detail: "the guest supervisor closed the connection mid-command".to_string(),
                });
            };

            match kind {
                // Written straight through: the bytes are the command's own,
                // and re-encoding them would corrupt non-text output.
                Kind::Stdout => write_all(out, &payload)?,
                Kind::Stderr => write_all(err, &payload)?,
                Kind::Exit => {
                    return serde_json::from_slice(&payload).map_err(|source| Error::Command {
                        program: "piebox",
                        detail: format!("malformed exit status from the guest: {source}"),
                    });
                }
                // A diagnostic, not the end of the exchange: the supervisor
                // still sends an exit status, and reporting that (127 for a
                // missing command, say) matches what a shell would do.
                Kind::Failure => {
                    let message = String::from_utf8_lossy(&payload);
                    write_all(err, format!("piebox guest: {message}\n").as_bytes())?;
                }
                Kind::Request => {
                    return Err(Error::Command {
                        program: "piebox",
                        detail: "the guest sent a request, which only the host may do".to_string(),
                    });
                }
            }
        }
    }
}

/// Writes guest output, tolerating a sink that is temporarily unwritable.
///
/// stdout can be non-blocking without piebox asking for it — anything sharing
/// the descriptor may have set the flag, libkrun's console included — and a
/// bare `write_all` then fails part-way through with `EAGAIN`, losing output.
fn write_all(sink: &mut impl Write, payload: &[u8]) -> Result<()> {
    let mut written = 0;
    while written < payload.len() {
        let remaining = payload.get(written..).unwrap_or_default();
        match sink.write(remaining) {
            Ok(0) => {
                return Err(Error::Command {
                    program: "piebox",
                    detail: "guest output sink stopped accepting data".to_string(),
                });
            }
            Ok(bytes) => written += bytes,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(err) => {
                return Err(Error::Command {
                    program: "piebox",
                    detail: format!("could not forward guest output: {err}"),
                });
            }
        }
    }
    loop {
        match sink.flush() {
            Ok(()) => return Ok(()),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(err) => {
                return Err(Error::Command {
                    program: "piebox",
                    detail: format!("could not flush guest output: {err}"),
                });
            }
        }
    }
}

/// Locates the cross-compiled supervisor on the host.
///
/// Looked up rather than embedded, because it is built for a *different* target
/// than piebox itself and embedding would make the host build depend on a
/// cross-compile having already happened.
///
/// # Errors
/// Fails when no candidate exists, naming how to build one.
pub fn guest_binary() -> Result<PathBuf> {
    guest_binary_with(std::env::var_os(ENV_GUEST_BIN))
}

/// [`guest_binary`] with the override supplied rather than read.
///
/// Takes it as an argument so tests never have to mutate the process
/// environment, which is shared with every other test in the binary.
fn guest_binary_with(explicit: Option<std::ffi::OsString>) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    // An explicit override is authoritative: falling back to a different
    // binary than the one asked for would be worse than failing, because the
    // wrong supervisor is exactly the bug this lookup exists to avoid.
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        let explicit = PathBuf::from(explicit);
        if !explicit.is_file() {
            return Err(Error::invalid(
                "guest supervisor",
                format!(
                    "{ENV_GUEST_BIN} points at {}, which does not exist; build it with `just piebox-guest`",
                    explicit.display()
                ),
            ));
        }
        if !is_guest_executable(&explicit) {
            return Err(Error::invalid(
                "guest supervisor",
                format!(
                    "{} is not a Linux {} executable; build it with `just piebox-guest`",
                    explicit.display(),
                    std::env::consts::ARCH
                ),
            ));
        }
        return Ok(explicit);
    }
    if let Ok(exe) = std::env::current_exe() {
        // The cross-compiled build comes first. A cargo layout also holds a
        // *host* build of the same crate next to piebox itself, and staging
        // that into the guest produces a baffling failure: the guest tries to
        // interpret a Mach-O as a shell script.
        //
        // Walk up rather than assuming a depth: piebox runs from
        // target/<profile>, but a test binary runs from target/<profile>/deps,
        // and an installed one from a bin directory.
        let cross = Path::new(&guest_target_triple())
            .join("release")
            .join("piebox-guest");
        candidates.extend(
            exe.ancestors()
                .take(4)
                .map(|ancestor| ancestor.join(&cross)),
        );
        // Next to piebox itself, which is how an installed pair is laid out.
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("piebox-guest"));
        }
    }

    let present: Vec<&PathBuf> = candidates.iter().filter(|path| path.is_file()).collect();
    if let Some(usable) = present.iter().find(|path| is_guest_executable(path)) {
        return Ok((*usable).clone());
    }
    // Distinguish "no build yet" from "built for the wrong platform", because
    // the second one otherwise surfaces as the guest failing to connect.
    if let Some(wrong) = present.first() {
        return Err(Error::invalid(
            "guest supervisor",
            format!(
                "{} is not a Linux {} executable; build it with `just piebox-guest`",
                wrong.display(),
                std::env::consts::ARCH
            ),
        ));
    }
    Err(Error::invalid(
        "guest supervisor",
        format!(
            "not found (tried {}); build it with `just piebox-guest` or set {ENV_GUEST_BIN}",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

/// Whether `path` is a 64-bit little-endian Linux ELF for this architecture.
///
/// Checked before staging: the guest has no way to report "wrong binary
/// format" other than failing to appear.
fn is_guest_executable(path: &Path) -> bool {
    use std::io::Read;
    let mut buffer = [0u8; 20];
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    // Only the header is needed; reading the whole ~500 KB to look at 20 bytes
    // would be wasteful.
    if file.read_exact(&mut buffer).is_err() {
        return false;
    }
    let header = &buffer[..];
    // e_ident: magic, then 64-bit class and little-endian data.
    if header.get(..4) != Some(b"\x7fELF") || header.get(4) != Some(&2) || header.get(5) != Some(&1)
    {
        return false;
    }
    // e_machine, at offset 18.
    let machine = match (header.get(18), header.get(19)) {
        (Some(low), Some(high)) => u16::from_le_bytes([*low, *high]),
        _ => return false,
    };
    machine == expected_elf_machine()
}

/// `e_machine` for the architecture piebox is running on.
const fn expected_elf_machine() -> u16 {
    if cfg!(target_arch = "aarch64") {
        0xB7 // EM_AARCH64
    } else if cfg!(target_arch = "x86_64") {
        0x3E // EM_X86_64
    } else {
        0 // EM_NONE: nothing will match, which is the honest answer
    }
}

/// Linux target matching this host's architecture.
///
/// musl so the result is fully static: the guest rootfs is an arbitrary image
/// and may have a different libc, or none.
pub fn guest_target_triple() -> String {
    format!("{}-unknown-linux-musl", std::env::consts::ARCH)
}

/// Copies the supervisor into the guest rootfs under a name unique to this run.
///
/// Unique on purpose. A single shared path cannot be made safe: replacing it
/// while another guest is executing it makes that guest fail with ENOENT (seen
/// under concurrent runs as "Couldn't execute '/.piebox/piebox-guest'"), and a
/// file already sitting there is untrustworthy anyway, because the guest can
/// write to this directory over virtio-fs and the supervisor receives every
/// command and every secret.
///
/// The returned guard removes the file when dropped.
///
/// # Errors
/// Fails when the rootfs is not writable or the binary cannot be copied.
pub fn stage_guest_binary(rootfs: &Path, binary: &Path) -> Result<StagedSupervisor> {
    let directory = rootfs.join(GUEST_STAGING_DIR);
    std::fs::create_dir_all(&directory).map_err(|err| Error::Command {
        program: "piebox",
        detail: format!("could not create {}: {err}", directory.display()),
    })?;
    remove_stale_supervisors(&directory);

    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("{GUEST_BINARY_PREFIX}{}-{serial}", std::process::id());

    let host_path = directory.join(&name);
    std::fs::copy(binary, &host_path).map_err(|err| Error::Command {
        program: "piebox",
        detail: format!(
            "could not stage {} into {}: {err}",
            binary.display(),
            host_path.display()
        ),
    })?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&host_path, std::fs::Permissions::from_mode(0o755)).map_err(
        |err| Error::Command {
            program: "piebox",
            detail: format!("could not make {} executable: {err}", host_path.display()),
        },
    )?;

    let guest_path = Path::new("/").join(GUEST_STAGING_DIR).join(&name);
    tracing::debug!(binary = %binary.display(), into = %host_path.display(), "staged guest supervisor");
    Ok(StagedSupervisor {
        host_path,
        guest_path,
    })
}

/// A staged supervisor, removed from the rootfs when dropped.
#[derive(Debug)]
pub struct StagedSupervisor {
    host_path: PathBuf,
    guest_path: PathBuf,
}

impl StagedSupervisor {
    /// Path to execute, as seen from inside the guest.
    pub fn guest_path(&self) -> &Path {
        &self.guest_path
    }
}

impl Drop for StagedSupervisor {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.host_path);
    }
}

/// Deletes leftovers from runs that were killed before they could clean up, so
/// the image does not accumulate a supervisor per crash.
fn remove_stale_supervisors(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // The fixed path used before staging became per-run.
        let legacy = name == "piebox-guest";
        if !legacy && !name.starts_with(GUEST_BINARY_PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age > STALE_AFTER);
        if legacy || stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guest_triple_targets_linux_and_static_musl() {
        let triple = guest_target_triple();
        assert!(triple.ends_with("-unknown-linux-musl"), "{triple}");
        assert!(triple.starts_with(std::env::consts::ARCH), "{triple}");
    }

    #[test]
    fn an_endpoint_is_private_to_its_owner_and_cleans_up() {
        use std::os::unix::fs::PermissionsExt;
        let socket;
        let directory;
        {
            let endpoint = Endpoint::bind().expect("bind");
            socket = endpoint.socket().to_path_buf();
            directory = socket.parent().expect("parent").to_path_buf();
            assert!(socket.exists(), "socket should exist while bound");
            let mode = directory.metadata().expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "directory must be owner-only");
        }
        assert!(!socket.exists(), "socket should be removed on drop");
        assert!(!directory.exists(), "directory should be removed on drop");
    }

    #[test]
    fn a_guest_that_never_connects_is_reported_not_waited_on_forever() {
        let endpoint = Endpoint::bind().expect("bind");
        let err = endpoint
            .accept(Duration::from_millis(150))
            .expect_err("nothing is going to connect");
        assert!(err.to_string().contains("did not connect"), "{err}");
    }

    /// Staging a host build into the guest fails as "the supervisor never
    /// connected", which says nothing. The format is checked instead.
    #[test]
    fn only_a_linux_executable_for_this_architecture_is_accepted() {
        let dir = std::env::temp_dir().join(format!("piebox-elf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");

        let mut header = vec![0u8; 20];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = 2; // 64-bit
        header[5] = 1; // little endian
        header[18..20].copy_from_slice(&expected_elf_machine().to_le_bytes());
        let good = dir.join("good");
        std::fs::write(&good, &header).expect("write");
        assert!(is_guest_executable(&good));

        // Same ELF, different machine.
        let mut other = header.clone();
        other[18..20].copy_from_slice(&0x1234u16.to_le_bytes());
        let wrong_arch = dir.join("wrong-arch");
        std::fs::write(&wrong_arch, other).expect("write");
        assert!(!is_guest_executable(&wrong_arch));

        // A Mach-O, i.e. the host build sitting next to piebox in target/.
        let macho = dir.join("macho");
        std::fs::write(&macho, [0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 1]).expect("write");
        assert!(!is_guest_executable(&macho));

        // A shell script, and something far too short to be either.
        let script = dir.join("script");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n").expect("write");
        assert!(!is_guest_executable(&script));
        let stub = dir.join("stub");
        std::fs::write(&stub, b"EL").expect("write");
        assert!(!is_guest_executable(&stub));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_wrong_platform_binary_is_named_as_such() {
        let dir = std::env::temp_dir().join(format!("piebox-wrong-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let binary = dir.join("piebox-guest");
        std::fs::write(&binary, [0xcf, 0xfa, 0xed, 0xfe]).expect("write");

        let err = guest_binary_with(Some(binary.clone().into_os_string()))
            .expect_err("a Mach-O must not be accepted");
        std::fs::remove_dir_all(&dir).ok();

        assert!(err.to_string().contains("not a Linux"), "{err}");
    }

    #[test]
    fn a_missing_guest_binary_says_how_to_build_one() {
        let err =
            guest_binary_with(Some("/nonexistent/piebox-guest".into())).expect_err("must fail");
        assert!(err.to_string().contains("just piebox-guest"), "{err}");
        // The tried paths are listed, so a wrong layout is diagnosable.
        assert!(
            err.to_string().contains("/nonexistent/piebox-guest"),
            "{err}"
        );
    }

    #[test]
    fn staging_gives_each_run_its_own_guest_path() {
        let temp = std::env::temp_dir().join(format!("piebox-stage-{}", std::process::id()));
        let rootfs = temp.join("rootfs");
        std::fs::create_dir_all(&rootfs).expect("rootfs");
        let binary = temp.join("piebox-guest");
        std::fs::write(&binary, b"#!/bin/true\n").expect("write");

        let first = stage_guest_binary(&rootfs, &binary).expect("stage");
        let guest_path = first.guest_path().to_path_buf();
        assert!(guest_path.is_absolute(), "{guest_path:?}");
        assert!(
            guest_path.starts_with(Path::new("/").join(GUEST_STAGING_DIR)),
            "{guest_path:?}"
        );

        let host_path = rootfs.join(guest_path.strip_prefix("/").expect("absolute"));
        assert!(host_path.is_file());

        // A second run must not collide with the first, which is still in use.
        let second = stage_guest_binary(&rootfs, &binary).expect("stage again");
        assert_ne!(first.guest_path(), second.guest_path());

        drop(first);
        assert!(
            !host_path.exists(),
            "the guard should remove the staged file"
        );

        std::fs::remove_dir_all(&temp).ok();
    }
}
