//! The supervisor that runs inside a piebox guest.
//!
//! It is the workload libkrun starts at boot. It connects out to the host over
//! vsock and then runs whatever the host asks for, streaming stdout and stderr
//! back as they arrive.
//!
//! Why it exists: the boot workload's arguments and environment travel on the
//! guest's kernel command line, which cannot carry a double quote, more than 32
//! arguments, more than ~1.5 KiB, or a literal `--`. Commands that arrive here
//! instead have none of those limits.
//!
//! Cross-compiled from the host with `rust-lld` (no C toolchain needed) and
//! staged into the guest rootfs before boot:
//! `cargo build -p piebox-guest --release --target aarch64-unknown-linux-musl`

#[cfg(target_os = "linux")]
mod mounts;
#[cfg(target_os = "linux")]
mod supervisor;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    supervisor::run()
}

/// The supervisor is Linux-only by nature: it talks AF_VSOCK and runs inside a
/// Linux guest. It still builds elsewhere so `cargo build --workspace` and CI
/// work on any host.
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!(
        "piebox-guest runs inside a Linux guest; build it for a Linux target:\n  \
         cargo build -p piebox-guest --release --target aarch64-unknown-linux-musl"
    );
    std::process::ExitCode::FAILURE
}
