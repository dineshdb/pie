//! The protocol between the piebox host and the supervisor inside the guest.
//!
//! Commands sent this way never touch the guest's kernel command line, so none
//! of that path's limits apply: quotes, arbitrary length, any number of
//! arguments and a literal `--` all survive.
//!
//! Requests are JSON, because they are structured and want to grow. Output is
//! carried in raw length-prefixed frames instead, because a command's stdout is
//! arbitrary bytes and need not be valid UTF-8 — JSON could not hold it without
//! lying about it.
//!
//! ```text
//! frame := kind:u8 | len:u32 (big endian) | payload[len]
//! ```

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use std::io::{self, Read, Write};

/// vsock port the supervisor connects to. Arbitrary but fixed: both ends have
/// to agree before either can say anything.
pub const SUPERVISOR_PORT: u32 = 1024;

/// Largest frame either side will send or accept (16 MiB).
///
/// A length prefix arriving from the other end is untrusted input; without a
/// ceiling a corrupt or hostile one would ask us to allocate anything it liked.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// What a frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// JSON [`Request`], host to guest.
    Request,
    /// Bytes the command wrote to stdout.
    Stdout,
    /// Bytes the command wrote to stderr.
    Stderr,
    /// The command finished; payload is a JSON [`Exit`].
    Exit,
    /// The supervisor itself failed; payload is a UTF-8 message.
    Failure,
}

impl Kind {
    const fn as_byte(self) -> u8 {
        match self {
            Self::Request => 1,
            Self::Stdout => 2,
            Self::Stderr => 3,
            Self::Exit => 4,
            Self::Failure => 5,
        }
    }

    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Request),
            2 => Some(Self::Stdout),
            3 => Some(Self::Stderr),
            4 => Some(Self::Exit),
            5 => Some(Self::Failure),
            _ => None,
        }
    }
}

/// A command for the guest to run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Request {
    /// Absolute path to the executable inside the guest.
    pub program: String,
    /// Arguments, excluding the program name.
    pub args: Vec<String>,
    /// Environment for the command. Replaces the supervisor's own.
    pub env: Vec<(String, String)>,
    /// Working directory inside the guest.
    pub cwd: Option<String>,
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Exit {
    /// Exit status, when the command exited normally.
    pub code: Option<i32>,
    /// Signal that killed it, when it did not.
    pub signal: Option<i32>,
}

/// The status a shell would report: `128 + signal` for a signal death.
impl From<Exit> for i32 {
    fn from(exit: Exit) -> Self {
        match (exit.code, exit.signal) {
            (Some(code), _) => code,
            (None, Some(signal)) => 128 + signal,
            (None, None) => 1,
        }
    }
}

/// Writes one frame.
///
/// # Errors
/// Propagates write failures, and refuses a payload above [`MAX_FRAME`].
pub fn write_frame<W: Write>(writer: &mut W, kind: Kind, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|l| *l as usize <= MAX_FRAME);
    let Some(len) = len else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "frame of {} bytes exceeds the {MAX_FRAME}-byte limit",
                payload.len()
            ),
        ));
    };
    writer.write_all(&[kind.as_byte()])?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

/// Writes a JSON-bodied frame.
///
/// # Errors
/// Fails if the value cannot be serialized, or the write fails.
pub fn write_json<W: Write, T: serde::Serialize>(
    writer: &mut W,
    kind: Kind,
    value: &T,
) -> io::Result<()> {
    let body =
        serde_json::to_vec(value).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    write_frame(writer, kind, &body)
}

/// Reads one frame, or `None` at a clean end of stream.
///
/// # Errors
/// Fails on a truncated frame, an unknown kind, or an oversized length.
pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<(Kind, Vec<u8>)>> {
    // The first byte decides between "hung up cleanly" and "corrupt stream":
    // read_exact cannot tell 0 bytes from 3, and treating a partial header as a
    // clean end of stream would silently discard the rest of the exchange.
    let mut kind_byte = [0u8; 1];
    match reader.read(&mut kind_byte) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::Interrupted => return read_frame(reader),
        Err(err) => return Err(err),
    }
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let header = [kind_byte[0], length[0], length[1], length[2], length[3]];

    let Some(kind) = Kind::from_byte(header[0]) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown frame kind {}", header[0]),
        ));
    };
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame claims {len} bytes, above the {MAX_FRAME}-byte limit"),
        ));
    }

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(Some((kind, payload)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_in_order() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, Kind::Stdout, b"hello").unwrap();
        write_frame(&mut buffer, Kind::Stderr, b"oops").unwrap();
        write_frame(&mut buffer, Kind::Stdout, b"").unwrap();

        let mut cursor = std::io::Cursor::new(buffer);
        assert_eq!(
            read_frame(&mut cursor).unwrap(),
            Some((Kind::Stdout, b"hello".to_vec()))
        );
        assert_eq!(
            read_frame(&mut cursor).unwrap(),
            Some((Kind::Stderr, b"oops".to_vec()))
        );
        // An empty payload is a real frame, not an end of stream.
        assert_eq!(
            read_frame(&mut cursor).unwrap(),
            Some((Kind::Stdout, vec![]))
        );
        assert_eq!(read_frame(&mut cursor).unwrap(), None);
    }

    /// Command output is arbitrary bytes; the protocol must not assume UTF-8.
    #[test]
    fn frames_carry_bytes_that_are_not_text() {
        let payload = vec![0x00, 0xff, 0xfe, b'\n', 0x80];
        let mut buffer = Vec::new();
        write_frame(&mut buffer, Kind::Stdout, &payload).unwrap();
        let mut cursor = std::io::Cursor::new(buffer);
        assert_eq!(
            read_frame(&mut cursor).unwrap(),
            Some((Kind::Stdout, payload))
        );
    }

    #[test]
    fn a_request_survives_what_a_command_line_could_not() {
        let request = Request {
            program: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo \"quoted\" && echo 'single'".to_string(),
                "--".to_string(),
                "x".repeat(100_000),
            ],
            env: vec![("A".to_string(), "b\"c".to_string())],
            cwd: Some("/tmp".to_string()),
        };
        let mut buffer = Vec::new();
        write_json(&mut buffer, Kind::Request, &request).unwrap();

        let mut cursor = std::io::Cursor::new(buffer);
        let (kind, body) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(kind, Kind::Request);
        let back: Request = serde_json::from_slice(&body).unwrap();
        assert_eq!(back.args[1], "echo \"quoted\" && echo 'single'");
        assert_eq!(back.args[2], "--");
        assert_eq!(back.args[3].len(), 100_000);
        assert_eq!(back.env[0].1, "b\"c");
    }

    /// A length prefix is untrusted: it must not be able to ask for a huge
    /// allocation, and a truncated frame must be an error rather than a hang.
    #[test]
    fn a_hostile_header_is_rejected() {
        let mut oversized = vec![Kind::Stdout.as_byte()];
        oversized.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(read_frame(&mut std::io::Cursor::new(oversized)).is_err());

        let unknown = vec![99, 0, 0, 0, 0];
        assert!(read_frame(&mut std::io::Cursor::new(unknown)).is_err());

        // Header promises 4 bytes, only 2 follow.
        let mut truncated = vec![Kind::Stdout.as_byte()];
        truncated.extend_from_slice(&4u32.to_be_bytes());
        truncated.extend_from_slice(b"ab");
        assert!(read_frame(&mut std::io::Cursor::new(truncated)).is_err());
    }

    /// A partial header means the stream was cut, not that the peer hung up
    /// cleanly — reporting the latter silently discards the rest of a command.
    #[test]
    fn a_truncated_header_is_an_error_not_an_end_of_stream() {
        for partial in [
            vec![Kind::Stdout.as_byte()],
            vec![Kind::Stdout.as_byte(), 0, 0],
        ] {
            let result = read_frame(&mut std::io::Cursor::new(partial.clone()));
            assert!(result.is_err(), "{partial:?} should be an error");
        }
        // Truly nothing, on the other hand, is a clean end of stream.
        assert_eq!(
            read_frame(&mut std::io::Cursor::new(Vec::new())).unwrap(),
            None
        );
    }

    #[test]
    fn writing_an_oversized_frame_is_refused() {
        let mut sink = Vec::new();
        let payload = vec![0u8; MAX_FRAME + 1];
        assert!(write_frame(&mut sink, Kind::Stdout, &payload).is_err());
        assert!(sink.is_empty(), "nothing should have been written");
    }

    #[test]
    fn exit_status_follows_shell_convention() {
        let code = |code, signal| i32::from(Exit { code, signal });
        assert_eq!(code(Some(0), None), 0);
        assert_eq!(code(Some(42), None), 42);
        // Killed by a signal: what a shell reports.
        assert_eq!(code(None, Some(9)), 137);
        // Neither: nothing better than a generic failure.
        assert_eq!(code(None, None), 1);
    }
    #[test]
    fn every_kind_round_trips_through_its_byte() {
        for kind in [
            Kind::Request,
            Kind::Stdout,
            Kind::Stderr,
            Kind::Exit,
            Kind::Failure,
        ] {
            assert_eq!(Kind::from_byte(kind.as_byte()), Some(kind));
        }
    }
}
