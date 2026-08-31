//! piebox CLI.

use clap::{Parser, Subcommand, ValueEnum};
use piebox::{ENV_LIBKRUN, Error, Feature, Libkrun, LogLevel};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "piebox",
    version,
    about = "libkrun-backed Linux microVM devbox for pie"
)]
struct Cli {
    /// libkrun log verbosity. Can only be set once per process.
    #[arg(long, global = true, value_enum)]
    log_level: Option<Verbosity>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report whether this host can run a piebox.
    Doctor,
}

#[derive(Clone, Copy, ValueEnum)]
enum Verbosity {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<Verbosity> for LogLevel {
    fn from(v: Verbosity) -> Self {
        match v {
            Verbosity::Off => Self::Off,
            Verbosity::Error => Self::Error,
            Verbosity::Warn => Self::Warn,
            Verbosity::Info => Self::Info,
            Verbosity::Debug => Self::Debug,
            Verbosity::Trace => Self::Trace,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor => doctor(cli.log_level.map(LogLevel::from)),
    }
}

/// Prints host capabilities. Exits non-zero when a requirement is missing.
fn doctor(log_level: Option<LogLevel>) -> ExitCode {
    let mut ok = true;

    match Libkrun::load() {
        Ok(lib) => {
            if let Some(level) = log_level
                && let Err(err) = lib.set_log_level(level)
            {
                println!("log level    {err}");
            }
            println!("libkrun      {}", lib.path().display());
            println!(
                "version      {}",
                library_version(lib.path()).unwrap_or_else(|| "unknown".to_string())
            );
            match lib.max_vcpus() {
                Ok(max) => println!("max vcpus    {max}"),
                Err(err) => {
                    println!("max vcpus    unknown ({err})");
                    ok = false;
                }
            }
            let supported: Vec<&str> = Feature::ALL
                .iter()
                .filter(|f| lib.has_feature(**f))
                .map(|f| f.name())
                .collect();
            println!("features     {}", supported.join(", "));
            // A guest needs a network backend and a root block/fs device.
            for required in [Feature::Net, Feature::Blk] {
                if !lib.has_feature(required) {
                    println!("MISSING      libkrun built without {}", required.name());
                    ok = false;
                }
            }
        }
        Err(err @ Error::LibraryLoad { .. }) => {
            println!("libkrun      NOT FOUND\n             {err}");
            ok = false;
        }
        Err(err) => {
            println!("libkrun      UNUSABLE\n             {err}");
            ok = false;
        }
    }

    // buildah materialises OCI images into the rootfs directory piebox boots.
    match which("buildah") {
        Some(path) => println!("buildah      {}", path.display()),
        None => {
            println!("buildah      NOT FOUND (needed to build guest rootfs images)");
            ok = false;
        }
    }

    ok &= report_hypervisor_entitlement();

    println!("\nhint         set {ENV_LIBKRUN} to pin a specific libkrun build");

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// On macOS, `hv_vm_create` needs the `com.apple.security.hypervisor`
/// entitlement, so an unsigned `cargo build` binary cannot start a VM at all.
/// Reported here because a bare "libkrun found" verdict would be misleading.
#[cfg(target_os = "macos")]
fn report_hypervisor_entitlement() -> bool {
    const ENTITLEMENT: &str = "com.apple.security.hypervisor";

    let Ok(exe) = std::env::current_exe() else {
        println!("entitlement  UNKNOWN (cannot locate current executable)");
        return false;
    };
    let output = std::process::Command::new("codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(&exe)
        .output();
    let Ok(output) = output else {
        println!("entitlement  UNKNOWN (codesign not available)");
        return false;
    };
    // codesign prints the entitlement plist to stdout, diagnostics to stderr.
    let plist = String::from_utf8_lossy(&output.stdout);
    if plist.contains(ENTITLEMENT) {
        println!("entitlement  {ENTITLEMENT}");
        return true;
    }
    println!(
        "entitlement  MISSING {ENTITLEMENT}\n             \
         sign it: just sign-piebox  (unsigned binaries cannot start a VM)"
    );
    false
}

#[cfg(not(target_os = "macos"))]
fn report_hypervisor_entitlement() -> bool {
    // Linux needs /dev/kvm access instead of a code-signing entitlement.
    let kvm = Path::new("/dev/kvm");
    if kvm.exists() {
        println!("kvm          {}", kvm.display());
        return true;
    }
    println!("kvm          MISSING /dev/kvm (libkrun needs KVM on Linux)");
    false
}

/// Reads the version out of a Homebrew-style Cellar path, e.g.
/// `/opt/homebrew/Cellar/libkrun/1.19.4/lib/libkrun.1.dylib` -> `1.19.4`.
///
/// A heuristic: libkrun exposes no version symbol. Non-Homebrew installs
/// simply report "unknown".
fn library_version(path: &Path) -> Option<String> {
    let resolved = path.canonicalize().ok()?;
    let mut components = resolved.components().peekable();
    while let Some(component) = components.next() {
        if component.as_os_str() == "libkrun"
            && let Some(next) = components.peek()
            && let Some(version) = next.as_os_str().to_str()
            && version.starts_with(|c: char| c.is_ascii_digit())
        {
            return Some(version.to_string());
        }
    }
    None
}

/// Minimal `which`, to avoid a dependency for one lookup.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}
