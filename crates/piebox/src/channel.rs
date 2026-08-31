//! Handing a [`VmSpec`] to the VMM child, and noticing when the parent dies.
//!
//! One pipe does both jobs. The parent writes the spec as a single JSON line
//! and then *keeps the write end open*; the child reads that line, then leaves
//! the same descriptor open in a watcher thread. A read of zero bytes can
//! therefore only mean the parent is gone, at which point the child shuts the
//! microVM down instead of outliving whoever asked for it.
//!
//! Why not command-line flags:
//! - argv is world-readable via `ps`, so `-e TOKEN=…` would leak;
//! - argument parsers consume `--`, so a guest argument of `--` cannot survive;
//! - every new field has to be re-serialized by hand, and forgetting one fails
//!   silently (that is exactly how `--log-level` came to be ignored).

use crate::error::{Error, Result};
use crate::vm::VmSpec;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;

/// Descriptor the child reads its spec from.
///
/// Fixed rather than negotiated: the child has to know where to look before it
/// can read anything, and 3 is the first descriptor after the standard three.
pub const SPEC_FD: RawFd = 3;

/// Exit status used when the child outlives its parent.
pub const EXIT_PARENT_GONE: i32 = 121;

impl VmSpec {
    /// Serializes the spec as the single line the child expects.
    ///
    /// # Errors
    /// Fails only if the spec cannot be represented as JSON.
    pub fn to_json_line(&self) -> Result<String> {
        let mut line =
            serde_json::to_string(self).map_err(|err| Error::invalid("spec", err.to_string()))?;
        line.push('\n');
        Ok(line)
    }

    /// Reads a spec from [`SPEC_FD`] and starts watching for the parent's exit.
    ///
    /// The returned guard must be kept alive for as long as the VM should live;
    /// dropping it stops the watcher.
    ///
    /// # Errors
    /// Fails when the descriptor is absent, closed early, or carries something
    /// that is not a spec.
    pub fn read_from_parent() -> Result<Self> {
        // Checked first: `from_raw_fd` on a closed descriptor is an IO-safety
        // violation, which aborts the process rather than returning an error.
        // SAFETY: fcntl with F_GETFD only inspects the descriptor.
        if unsafe { libc::fcntl(SPEC_FD, libc::F_GETFD) } == -1 {
            return Err(Error::Command {
                program: "piebox __vmm",
                detail: format!(
                    "fd {SPEC_FD} is not open; __vmm is internal and is spawned by `piebox run`"
                ),
            });
        }
        // SAFETY: the parent dup2'd the pipe onto this descriptor before exec,
        // and nothing else in this process owns it.
        let pipe = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(SPEC_FD) };
        let mut reader = BufReader::new(pipe);

        let mut line = String::new();
        let read = reader.read_line(&mut line).map_err(|err| Error::Command {
            program: "piebox __vmm",
            detail: format!("could not read the VM spec from fd {SPEC_FD}: {err}"),
        })?;
        if read == 0 {
            return Err(Error::Command {
                program: "piebox __vmm",
                detail: format!("no VM spec arrived on fd {SPEC_FD}"),
            });
        }

        let spec: Self = serde_json::from_str(line.trim_end()).map_err(|err| Error::Command {
            program: "piebox __vmm",
            detail: format!("malformed VM spec: {err}"),
        })?;

        watch_parent(reader);
        Ok(spec)
    }
}

/// Shuts this process down when the pipe's other end closes.
///
/// `krun_start_enter` never returns, so the VM cannot poll for anything: the
/// watcher has to be a thread, and it has to end the process outright.
fn watch_parent<R: Read + Send + 'static>(mut reader: R) {
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte) {
                // EOF: the parent is gone and nobody is waiting for this VM.
                Ok(0) => break,
                // The parent writes nothing after the spec, but tolerate it.
                Ok(_) => continue,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        eprintln!("piebox: parent exited, shutting the microVM down");
        // _exit, not exit: libkrun's vCPU threads are still running, and there
        // is nothing this process needs to unwind or flush.
        // SAFETY: _exit is async-signal-safe and never returns.
        unsafe { libc::_exit(EXIT_PARENT_GONE) };
    });
}

/// Arranges for `reader` to appear as [`SPEC_FD`] in the child.
///
/// # Safety
/// Registers a `pre_exec` hook, so the closure must stay async-signal-safe: it
/// only calls `dup2`/`fcntl`, which are.
pub fn attach_spec_fd(command: &mut std::process::Command, reader: &std::io::PipeReader) {
    let source = reader.as_raw_fd();
    // SAFETY: the closure runs between fork and exec and touches nothing but
    // the two async-signal-safe syscalls below.
    unsafe {
        command.pre_exec(move || {
            if source == SPEC_FD {
                // Already in place; just make sure exec does not close it.
                let flags = libc::fcntl(SPEC_FD, libc::F_GETFD);
                if flags == -1
                    || libc::fcntl(SPEC_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
                return Ok(());
            }
            // dup2 clears FD_CLOEXEC on the new descriptor, which is what lets
            // it survive the exec.
            if libc::dup2(source, SPEC_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Sends `spec` to a spawned child, keeping the pipe open afterwards.
///
/// The returned writer is the child's lifeline: hold it until the child has
/// been waited for, and drop it to tell the child to shut down.
///
/// # Errors
/// Fails if the spec cannot be serialized or the child closed the pipe.
pub fn send_spec(mut writer: std::io::PipeWriter, spec: &VmSpec) -> Result<std::io::PipeWriter> {
    let line = spec.to_json_line()?;
    writer
        .write_all(line.as_bytes())
        .and_then(|()| writer.flush())
        .map_err(|err| Error::Command {
            program: "piebox __vmm",
            detail: format!("could not send the VM spec: {err}"),
        })?;
    Ok(writer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> VmSpec {
        let mut spec = VmSpec::new("/", "/bin/echo").expect("spec");
        spec.args = vec!["a".to_string(), "--".to_string(), "b".to_string()];
        spec.env.push(("TOKEN".to_string(), "s3cret".to_string()));
        spec
    }

    #[test]
    fn a_spec_survives_the_json_round_trip() {
        let spec = spec();
        let line = spec.to_json_line().expect("serialize");
        assert!(line.ends_with('\n'), "must be one line: {line:?}");
        assert_eq!(line.matches('\n').count(), 1);

        let back: VmSpec = serde_json::from_str(line.trim_end()).expect("deserialize");
        assert_eq!(back.exec, spec.exec);
        // The values argv could not carry: a literal `--` and a secret.
        assert_eq!(back.args, vec!["a", "--", "b"]);
        assert!(back.env.iter().any(|(k, v)| k == "TOKEN" && v == "s3cret"));
        assert_eq!(back.vcpus, spec.vcpus);
        assert_eq!(back.ram, spec.ram);
    }

    /// A spec from another process is untrusted input; the newtypes must still
    /// reject impossible values rather than passing them to libkrun.
    #[test]
    fn a_malformed_spec_is_rejected_on_the_way_in() {
        let valid = spec().to_json_line().expect("serialize");
        for bad in [
            valid.replace("\"vcpus\":2", "\"vcpus\":0"),
            valid.replace("\"ram\":1024", "\"ram\":0"),
            valid.replace("\"ram\":1024", "\"ram\":4294967295"),
        ] {
            assert!(
                serde_json::from_str::<VmSpec>(bad.trim_end()).is_err(),
                "must be rejected: {bad}"
            );
        }
    }
}
