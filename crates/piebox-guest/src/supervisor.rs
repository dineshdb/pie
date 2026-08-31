//! Linux side of the supervisor: connect over vsock, then serve requests.

use piebox_proto::{Exit, Kind, Request, SUPERVISOR_PORT, read_frame, write_frame, write_json};
use std::io::{BufReader, Read};
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, ExitCode, Stdio};

/// The host's address on a vsock. Fixed by the virtio-vsock spec.
const VMADDR_CID_HOST: u32 = 2;

pub fn run() -> ExitCode {
    let stream = match connect_to_host() {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("piebox-guest: could not reach the host: {err}");
            return ExitCode::FAILURE;
        }
    };

    // One connection carries every request, so a single boot can serve many
    // commands.
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(err) => {
            eprintln!("piebox-guest: could not split the connection: {err}");
            return ExitCode::FAILURE;
        }
    });
    let mut writer = stream;

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            // The host closed the connection: the VM's work is done.
            Ok(None) => return ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("piebox-guest: bad frame from the host: {err}");
                return ExitCode::FAILURE;
            }
        };

        let (kind, payload) = frame;
        if kind != Kind::Request {
            fail(&mut writer, &format!("expected a request, got {kind:?}"));
            continue;
        }

        match serde_json::from_slice::<Request>(&payload) {
            Ok(request) => execute(&request, &mut writer),
            Err(err) => fail(&mut writer, &format!("malformed request: {err}")),
        }
    }
}

/// Reports a failure *and* an exit status.
///
/// Every request must be answered by exactly one `Exit`: the host reads until
/// it sees one, so a bare `Failure` would leave it waiting forever.
fn fail<W: std::io::Write>(writer: &mut W, message: &str) {
    let _ = write_frame(writer, Kind::Failure, message.as_bytes());
    let _ = write_json(
        writer,
        Kind::Exit,
        &Exit {
            code: Some(1),
            signal: None,
        },
    );
}

/// Runs one request, streaming its output back as it is produced.
fn execute<W: std::io::Write>(request: &Request, writer: &mut W) {
    let mut command = Command::new(&request.program);
    command
        .args(&request.args)
        // The host decides the environment completely; nothing of the
        // supervisor's own leaks into the command.
        .env_clear()
        .envs(request.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }

    // Otherwise a bad cwd surfaces as ENOENT and reads as "the program does
    // not exist", which sends the caller looking in the wrong place.
    if let Some(cwd) = &request.cwd
        && !std::path::Path::new(cwd).is_dir()
    {
        fail(
            writer,
            &format!("working directory {cwd} does not exist in the guest"),
        );
        return;
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            let message = format!("could not run {}: {err}", request.program);
            let _ = write_frame(writer, Kind::Failure, message.as_bytes());
            // Report a status too, so the host is never left waiting.
            let _ = write_json(
                writer,
                Kind::Exit,
                &Exit {
                    code: Some(status_for(&err)),
                    signal: None,
                },
            );
            return;
        }
    };

    // stdout and stderr are drained on separate threads: a command that fills
    // one pipe while the reader waits on the other would otherwise deadlock.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel::<(Kind, Vec<u8>)>();

    let stdout_thread = stdout.map(|pipe| spawn_pump(pipe, Kind::Stdout, tx.clone()));
    let stderr_thread = stderr.map(|pipe| spawn_pump(pipe, Kind::Stderr, tx.clone()));
    drop(tx);

    let mut host_gone = false;
    for (kind, chunk) in rx {
        if write_frame(writer, kind, &chunk).is_err() {
            host_gone = true;
            break;
        }
    }
    if host_gone {
        // Nobody is reading any more. Without this the pumps keep buffering
        // whatever the command produces and the joins below never return.
        let _ = child.kill();
    }
    if let Some(thread) = stdout_thread {
        let _ = thread.join();
    }
    if let Some(thread) = stderr_thread {
        let _ = thread.join();
    }

    let exit = match child.wait() {
        Ok(status) => Exit {
            code: status.code(),
            signal: signal_of(&status),
        },
        Err(err) => {
            let message = format!("could not wait for {}: {err}", request.program);
            let _ = write_frame(writer, Kind::Failure, message.as_bytes());
            Exit {
                code: Some(1),
                signal: None,
            }
        }
    };
    let _ = write_json(writer, Kind::Exit, &exit);
}

/// Forwards one pipe to the frame channel until it closes.
fn spawn_pump<R: Read + Send + 'static>(
    mut pipe: R,
    kind: Kind,
    tx: std::sync::mpsc::Sender<(Kind, Vec<u8>)>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => return,
                Ok(read) => {
                    let chunk = buffer.get(..read).unwrap_or_default().to_vec();
                    if tx.send((kind, chunk)).is_err() {
                        return;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    })
}

fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

/// Mirrors a shell: 127 when the program is missing, 126 when it cannot run.
fn status_for(err: &std::io::Error) -> i32 {
    match err.kind() {
        std::io::ErrorKind::NotFound => 127,
        _ => 126,
    }
}

/// Opens the vsock connection to the host.
///
/// Hand-rolled rather than pulled from a crate: it is one `socket` and one
/// `connect`, and this binary is staged into a guest rootfs, so every
/// dependency is weight that has to be cross-compiled and shipped.
fn connect_to_host() -> std::io::Result<std::os::unix::net::UnixStream> {
    let mut last = None;
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        match connect_once() {
            Ok(stream) => return Ok(stream),
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("vsock connect failed")))
}

fn connect_once() -> std::io::Result<std::os::unix::net::UnixStream> {
    // SAFETY: a plain socket(2) call with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor this function owns.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    // Written as consts: `unwrap_or_default()` here would substitute
    // AF_UNSPEC and a zero length, i.e. quietly connect to nothing.
    const FAMILY: u16 = libc::AF_VSOCK as u16;
    const ADDR_LEN: u32 = std::mem::size_of::<libc::sockaddr_vm>() as u32;

    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = FAMILY;
    addr.svm_cid = VMADDR_CID_HOST;
    addr.svm_port = SUPERVISOR_PORT;

    // SAFETY: `addr` is a correctly sized, fully initialised sockaddr_vm, and
    // the length passed matches the type.
    let connected = unsafe {
        libc::connect(
            fd,
            std::ptr::from_ref(&addr).cast::<libc::sockaddr>(),
            ADDR_LEN,
        )
    };
    if connected < 0 {
        return Err(std::io::Error::last_os_error());
    }

    // A vsock stream behaves like any other SOCK_STREAM, so the standard
    // UnixStream is a fine wrapper for read/write.
    Ok(std::os::unix::net::UnixStream::from(owned))
}
