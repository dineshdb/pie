//! piebox CLI.

use clap::{Parser, Subcommand, ValueEnum};
use piebox::{
    ContainerStorage, ENV_LIBKRUN, Error, Feature, Libkrun, LogLevel, RamMib, Vcpus, VmSpec,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "piebox",
    version,
    about = "libkrun-backed Linux microVM devbox for pie",
    after_help = "\
Flags follow docker where they mean the same thing (-v, -e, -w, -m). The guest
command follows `--`:

    piebox run -- /bin/sh -c 'uname -r'
    piebox run --cpus 4 -m 4g -- /usr/bin/make -j4
    piebox run -v .:/work -w /work -- cargo test
    piebox run -v ~/src:/src:ro -- /bin/ls /src

Unlike docker, a bare -m value is MiB rather than bytes, and secrets are better
passed by name than by value, since a command line is readable by every user on
the machine:

    TOKEN=... piebox run -e TOKEN -- ./deploy"
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

    /// Run a command inside a piebox guest, given after `--`.
    Run {
        #[command(flatten)]
        vm: VmArgs,
    },

    /// Become the VM. Internal: `run` re-execs piebox with this.
    ///
    /// It exists because `krun_start_enter` never returns — it takes over the
    /// process — so the VM cannot share a process with anything that has to
    /// outlive it.
    /// It takes no arguments: the spec arrives on a pipe, because argv is
    /// visible to every user on the machine and cannot carry a literal `--`.
    #[command(hide = true, name = "__vmm")]
    Vmm,
}

/// Guest sizing and source, shared by `run` and `__vmm`.
#[derive(clap::Args, Clone, Debug)]
struct VmArgs {
    /// Number of guest CPUs.
    ///
    /// A whole number of vCPUs, unlike docker's fractional share.
    #[arg(long, default_value_t = 2, env = "PIEBOX_CPUS")]
    cpus: u8,

    /// Guest memory, as a plain number of MiB or with a `m`/`g` suffix.
    ///
    /// A bare number is MiB, not bytes as docker reads it: this sizes a VM, and
    /// `-m 1024` meaning one kilobyte would be a trap.
    #[arg(
        long,
        short = 'm',
        default_value = "1024",
        env = "PIEBOX_MEMORY",
        value_name = "SIZE"
    )]
    memory: String,

    /// Working directory inside the guest.
    #[arg(long, short = 'w')]
    workdir: Option<PathBuf>,

    /// Buildah container whose root filesystem the guest boots.
    #[arg(
        long,
        default_value = "ubuntu-working-container",
        env = "PIEBOX_CONTAINER"
    )]
    container: String,

    /// Host directory to boot directly, instead of resolving a container.
    #[arg(long, conflicts_with = "container")]
    rootfs_path: Option<PathBuf>,

    /// Expose a host directory to the guest, as HOST:GUEST or HOST:GUEST:ro.
    ///
    /// Implies --supervised: attaching the device is all the host can do, and
    /// only the in-guest supervisor can mount it.
    #[arg(long = "volume", short = 'v', value_name = "HOST:GUEST[:ro]")]
    volumes: Vec<String>,

    /// Run the command through the in-guest supervisor instead of as the boot
    /// workload.
    ///
    /// Slower to start, but nothing goes near the guest's kernel command line,
    /// so quotes, long commands, many arguments and a literal `--` all work,
    /// and stdout and stderr stay separate.
    #[arg(long)]
    supervised: bool,

    /// Environment variable for the guest, as KEY=VALUE, or just KEY to pass
    /// through piebox's own value.
    ///
    /// Prefer bare KEY for anything secret: a value written as KEY=VALUE is
    /// visible to every user on the machine in *this* process's `ps` output.
    #[arg(long = "env", short = 'e', value_name = "KEY[=VALUE]")]
    env: Vec<String>,
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

impl Verbosity {
    /// The flag value clap parses back into this variant.
    const fn to_possible_value_name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
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
    // The guest command is taken from argv directly rather than through clap,
    // which consumes a `--` token wherever it appears and would silently
    // truncate a command like `cargo test -- --nocapture`.
    let (piebox_args, guest_command) = split_at_first_double_dash(std::env::args_os());

    let cli = Cli::parse_from(piebox_args);
    let log_level = cli.log_level.map(LogLevel::from);
    match cli.command {
        Command::Doctor => doctor(log_level),
        Command::Run { vm } => match run(&vm, &guest_command, cli.log_level) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("{}", report(&err));
                ExitCode::FAILURE
            }
        },
        // `boot` only returns on failure; on success it replaces this process.
        Command::Vmm => match become_vm(log_level) {
            Err(err) => {
                eprintln!("piebox vmm: {err}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Renders an error without doubling the `piebox:` prefix that
/// [`Error::Command`] already carries.
fn report(err: &Error) -> String {
    let text = err.to_string();
    if text.starts_with("piebox") {
        text
    } else {
        format!("piebox: {text}")
    }
}

/// Splits `piebox … -- guest command` into its two halves.
///
/// Only the first `--` separates; any later one belongs to the guest.
fn split_at_first_double_dash<I>(args: I) -> (Vec<std::ffi::OsString>, Vec<std::ffi::OsString>)
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut ours = Vec::new();
    let mut guest = Vec::new();
    let mut seen_separator = false;
    for arg in args {
        if seen_separator {
            guest.push(arg);
        } else if arg == "--" {
            seen_separator = true;
        } else {
            ours.push(arg);
        }
    }
    (ours, guest)
}

/// Builds and validates the spec. Used by both sides: the parent to refuse bad
/// input before spawning, the child because it is the one that boots.
fn build_spec(
    vm: &VmArgs,
    rootfs: &std::path::Path,
    command: &[std::ffi::OsString],
) -> Result<VmSpec, Error> {
    let (exec, args) = command.split_first().ok_or_else(|| {
        Error::invalid(
            "command",
            "no command given; put it after `--`, e.g. `piebox run -- /bin/sh -c date`",
        )
    })?;

    check_workdir(vm, rootfs, &[])?;

    let mut spec = VmSpec::new(rootfs, utf8(exec)?)?;
    spec.vcpus = Vcpus::new(vm.cpus)?;
    spec.ram = parse_memory(&vm.memory)?;
    spec.workdir = vm.workdir.clone();
    spec.args = args.iter().map(utf8).collect::<Result<_, _>>()?;
    spec.env.extend(resolve_env(&vm.env)?);
    spec.validate()?;
    Ok(spec)
}

/// Refuses a working directory that does not exist inside the guest.
///
/// libkrun's init lets a failed chdir slide and runs the workload in `/`, and
/// the supervisor would report the failure as if the *program* were missing, so
/// neither path can be trusted to notice.
fn check_workdir(
    vm: &VmArgs,
    rootfs: &std::path::Path,
    mounts: &[piebox::Mount],
) -> Result<(), Error> {
    if let Some(workdir) = &vm.workdir
        // A workdir inside a mount does not exist in the image and is not
        // supposed to: the mount provides it once the guest is running.
        && !mounts
            .iter()
            .any(|mount| workdir.starts_with(&mount.guest_path))
        && let Ok(relative) = workdir.strip_prefix("/")
        && !rootfs.join(relative).is_dir()
    {
        return Err(Error::invalid(
            "workdir",
            format!("{} does not exist in the guest", workdir.display()),
        ));
    }
    Ok(())
}

/// Resolves `KEY=VALUE` entries, and bare `KEY` from piebox's own environment.
fn resolve_env(entries: &[String]) -> Result<Vec<(String, String)>, Error> {
    entries
        .iter()
        .map(|entry| match entry.split_once('=') {
            Some((key, value)) => Ok((key.to_string(), value.to_string())),
            // Bare KEY: take the value from piebox's own environment, so a
            // secret never has to appear on anybody's command line.
            None => {
                let value = std::env::var(entry).map_err(|_| {
                    Error::invalid(
                        "env",
                        format!("{entry:?} is not set in piebox's environment"),
                    )
                })?;
                Ok((entry.clone(), value))
            }
        })
        .collect()
}

/// Parses a memory size: a plain number of MiB, or one with a `m`/`g` suffix.
///
/// Docker reads a bare number as *bytes*; piebox reads MiB, because this sizes a
/// VM and `-m 1024` meaning a kilobyte would be a trap rather than a nicety.
fn parse_memory(raw: &str) -> Result<RamMib, Error> {
    let text = raw.trim().to_ascii_lowercase();
    let (digits, multiplier) = match text.as_str() {
        rest if rest.ends_with("gib") => (rest.trim_end_matches("gib"), 1024),
        rest if rest.ends_with("mib") => (rest.trim_end_matches("mib"), 1),
        rest if rest.ends_with('g') => (rest.trim_end_matches('g'), 1024),
        rest if rest.ends_with('m') => (rest.trim_end_matches('m'), 1),
        rest => (rest, 1),
    };
    let value: u32 = digits.trim().parse().map_err(|_| {
        Error::invalid(
            "memory",
            format!("{raw:?} is not a size; use MiB (1024) or a suffix (1g, 512m)"),
        )
    })?;
    let mib = value.checked_mul(multiplier).ok_or_else(|| {
        Error::invalid("memory", format!("{raw:?} overflows the addressable range"))
    })?;
    RamMib::new(mib)
}

/// A mount's guest path as UTF-8, which the protocol requires.
fn guest_path_str(mount: &piebox::Mount) -> Result<String, Error> {
    mount
        .guest_path
        .to_str()
        .map(ToString::to_string)
        .ok_or_else(|| {
            Error::invalid(
                "mount",
                format!(
                    "guest path {} is not valid UTF-8",
                    mount.guest_path.display()
                ),
            )
        })
}

/// Converts an argv entry, which libkrun and the guest both require as UTF-8.
fn utf8(value: &std::ffi::OsString) -> Result<String, Error> {
    value.to_str().map(ToString::to_string).ok_or_else(|| {
        Error::invalid(
            "command",
            format!("{value:?} is not valid UTF-8, which the guest requires"),
        )
    })
}

/// Runs the command through the guest supervisor.
///
/// The VM boots the supervisor instead of the command, so the command itself
/// travels over a socket and is subject to none of the kernel command line's
/// limits.
fn run_supervised(
    vm: &VmArgs,
    rootfs: &std::path::Path,
    mounts: Vec<piebox::Mount>,
    command: &[std::ffi::OsString],
    log_level: Option<Verbosity>,
) -> Result<ExitCode, Error> {
    let (program, args) = command.split_first().ok_or_else(|| {
        Error::invalid(
            "command",
            "no command given; put it after `--`, e.g. `piebox run --supervised -- /bin/sh -c date`",
        )
    })?;
    check_workdir(vm, rootfs, &mounts)?;

    // Created before the guest boots and removed after it exits, so a mount
    // point piebox invented does not stay in the image forever.
    let _mount_points = piebox::MountPoints::create(rootfs, &mounts)?;

    let request = piebox::Request {
        program: utf8(program)?,
        args: args.iter().map(utf8).collect::<Result<_, _>>()?,
        env: guest_env(vm)?,
        cwd: vm
            .workdir
            .as_ref()
            .map(|dir| dir.to_string_lossy().into_owned()),
        // The guest is told the tag and where to put it; the host attaches the
        // matching device below, and the two agree by index.
        mounts: piebox::tagged(&mounts)
            .into_iter()
            .map(|(tag, mount)| {
                Ok(piebox_proto::Mount {
                    tag,
                    target: guest_path_str(mount)?,
                    read_only: mount.read_only,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?,
    };

    let binary = piebox::guest_binary()?;
    // Held until the VM has exited: dropping it removes the staged file.
    let staged = piebox::stage_guest_binary(rootfs, &binary)?;
    let endpoint = piebox::Endpoint::bind()?;

    // The supervisor is the boot workload, and it takes no arguments, so the
    // kernel command line stays short no matter what the command looks like.
    let mut spec = VmSpec::new(rootfs, staged.guest_path())?;
    spec.vcpus = Vcpus::new(vm.cpus)?;
    spec.ram = parse_memory(&vm.memory)?;
    spec.vsock = Some(piebox::Vsock {
        port: piebox::SUPERVISOR_PORT,
        socket: endpoint.socket().to_path_buf(),
    });
    spec.mounts = mounts;
    spec.validate()?;

    let lib = Libkrun::load()?;
    // The console goes to a file: it is only boot chatter here, and leaving it
    // on stdout would let libkrun make that descriptor non-blocking underneath
    // us. Kept so a guest that never connects can still be explained.
    let console = Console::File(endpoint.socket().with_extension("console"));
    let mut vmm = spawn_vmm(&lib, &spec, log_level, &console)?;

    // Boot plus connect; generous because a cold rootfs can be slow to read.
    let mut supervisor = match endpoint.accept(std::time::Duration::from_secs(30)) {
        Ok(supervisor) => supervisor,
        Err(err) => {
            let _ = vmm.child.kill();
            let _ = vmm.wait();
            return Err(with_console_tail(err, &console));
        }
    };
    let exit = supervisor.run(
        &request,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );

    // Dropping the connection tells the supervisor to exit, which shuts the VM
    // down; the lifeline then closes on its own.
    drop(supervisor);
    let _ = vmm.wait();
    // Only now: the guest was executing this file until the VM exited.
    drop(staged);
    if let Console::File(path) = &console {
        let _ = std::fs::remove_file(path);
    }

    let exit = exit?;
    Ok(ExitCode::from(u8::try_from(i32::from(exit)).unwrap_or(1)))
}

/// Adds the guest's console output to an error, which is usually where the
/// reason a supervisor never appeared is written.
fn with_console_tail(err: Error, console: &Console) -> Error {
    let Console::File(path) = console else {
        return err;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return err;
    };
    let tail: Vec<&str> = text.lines().rev().take(10).collect();
    if tail.is_empty() {
        return err;
    }
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    Error::Command {
        program: "piebox",
        detail: format!("{err}\nguest console:\n  {}", tail.join("\n  ")),
    }
}

/// Environment for a guest command: the same defaults the boot path uses, plus
/// whatever was asked for.
///
/// Shares [`VmSpec::default_env`] rather than restating it, because the two
/// lists had already drifted apart once.
fn guest_env(vm: &VmArgs) -> Result<Vec<(String, String)>, Error> {
    let mut env = VmSpec::default_env();
    env.extend(resolve_env(&vm.env)?);
    Ok(env)
}

/// Resolves the rootfs, then re-execs piebox as the VM and waits for it.
fn run(
    vm: &VmArgs,
    command: &[std::ffi::OsString],
    log_level: Option<Verbosity>,
) -> Result<ExitCode, Error> {
    // Parsed first: a typo'd --volume should not leave a buildah mount
    // reference behind on the way to being rejected.
    let mounts = vm
        .volumes
        .iter()
        .map(|raw| raw.parse::<piebox::Mount>())
        .collect::<Result<Vec<_>, _>>()?;
    piebox::check_mount_collisions(&mounts)?;

    let rootfs = match &vm.rootfs_path {
        Some(path) => path.clone(),
        None => {
            let storage = ContainerStorage::from_env();
            piebox::container_rootfs(&storage, &vm.container)?
        }
    };

    // Mounts can only be set up from inside the guest, so asking for one
    // selects the supervised path rather than being refused.
    if vm.supervised || !mounts.is_empty() {
        return run_supervised(vm, &rootfs, mounts, command, log_level);
    }

    // Refuse bad input before a VM process exists: the error is clearer, and
    // it does not need a hypervisor to report it.
    let spec = build_spec(vm, &rootfs, command)?;

    // Load libkrun here as well: it fails early with a good message, and the
    // child has to be told where the library and its payload live.
    let lib = Libkrun::load()?;
    let mut vmm = spawn_vmm(&lib, &spec, log_level, &Console::Inherit)?;
    Ok(exit_code_from(vmm.wait()?))
}

/// A running VM process, and the pipe keeping it alive.
struct Vmm {
    child: std::process::Child,
    /// Closing this tells the VM its parent is gone, so it must not be dropped
    /// before the child has been waited for.
    lifeline: Option<std::io::PipeWriter>,
}

impl Drop for Vmm {
    fn drop(&mut self) {
        // Only reached when `wait` was skipped, e.g. an early `?`. Without this
        // the VM keeps running until the lifeline closes, and the process stays
        // a zombie until piebox itself exits.
        if self.lifeline.is_some() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Vmm {
    fn wait(&mut self) -> Result<std::process::ExitStatus, Error> {
        let status = self.child.wait().map_err(|err| Error::Command {
            program: "piebox __vmm",
            detail: format!("could not wait for the VM process: {err}"),
        })?;
        drop(self.lifeline.take());
        Ok(status)
    }
}

/// Where the guest's console output goes.
enum Console {
    /// Straight to this process's stdout, for an interactive guest.
    Inherit,
    /// To a file. Used when the host needs its own stdout back: libkrun's
    /// console shares the descriptor and sets it non-blocking, after which
    /// piebox's own writes start failing with EAGAIN.
    File(PathBuf),
}

/// Re-execs piebox as the VM process and hands it the spec over a pipe.
fn spawn_vmm(
    lib: &Libkrun,
    spec: &VmSpec,
    log_level: Option<Verbosity>,
    console: &Console,
) -> Result<Vmm, Error> {
    let exe = std::env::current_exe().map_err(|err| Error::Command {
        program: "piebox",
        detail: format!("cannot locate own executable: {err}"),
    })?;

    let (reader, writer) = std::io::pipe().map_err(|err| Error::Command {
        program: "piebox",
        detail: format!("could not create the spec pipe: {err}"),
    })?;

    let mut child = std::process::Command::new(exe);
    child.envs(piebox::loader_env(lib.path()));
    child.arg("__vmm");
    if let Some(level) = log_level {
        child.arg("--log-level").arg(level.to_possible_value_name());
    }
    // The spec travels on a pipe: nothing about the guest — its command, its
    // arguments, its environment — appears in argv, where `ps` would show it.
    piebox::attach_spec_fd(&mut child, &reader);

    if let Console::File(path) = console {
        let log = std::fs::File::create(path).map_err(|err| Error::Command {
            program: "piebox",
            detail: format!("could not create {}: {err}", path.display()),
        })?;
        let log_err = log.try_clone().map_err(|err| Error::Command {
            program: "piebox",
            detail: format!("could not duplicate {}: {err}", path.display()),
        })?;
        // All three, not just stdout: libkrun's console sets O_NONBLOCK on the
        // descriptors it is handed, and because that flag belongs to the shared
        // open file description it propagates back out past piebox to whatever
        // invoked it — an interactive terminal included.
        child.stdin(std::process::Stdio::null());
        child.stdout(std::process::Stdio::from(log));
        child.stderr(std::process::Stdio::from(log_err));
    }

    let spawned = child.spawn().map_err(|err| Error::Command {
        program: "piebox __vmm",
        detail: format!("could not start the VM process: {err}"),
    })?;
    // The child has its own duplicate now; holding this copy open would keep
    // the guard from ever seeing EOF.
    drop(reader);

    let lifeline = piebox::send_spec(writer, spec)?;
    Ok(Vmm {
        child: spawned,
        lifeline: Some(lifeline),
    })
}

/// Translates the VM process's exit status, naming libkrun's reserved codes.
fn exit_code_from(status: std::process::ExitStatus) -> ExitCode {
    let Some(code) = status.code() else {
        // The VM *process* died by signal (a guest workload killed by a signal
        // arrives as 128+n from libkrun's init instead, so this is a host-side
        // failure). Follow the shell convention rather than discarding which.
        use std::os::unix::process::ExitStatusExt;
        let signal = status.signal().unwrap_or_default();
        eprintln!("piebox: the VM process was killed by signal {signal}");
        return ExitCode::from(u8::try_from(128 + signal).unwrap_or(1));
    };
    // These are libkrun init's own failures, not the workload's exit status.
    match code {
        piebox::EXIT_INIT_SETUP_FAILED => {
            eprintln!("piebox: guest init could not set up the environment (125)");
        }
        piebox::EXIT_EXEC_FAILED => {
            eprintln!("piebox: guest command found but could not be executed (126)");
        }
        piebox::EXIT_EXEC_NOT_FOUND => {
            // libkrun's init reserves 127, but a workload may also exit 127 by
            // itself and there is no channel that distinguishes the two.
            eprintln!(
                "piebox: exit 127 — libkrun's init uses this for \"command not found in the \
                 rootfs\"; it can also be the workload's own status"
            );
        }
        _ => {}
    }
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Configures and enters the microVM. Returns only on failure.
///
/// The spec arrives on a pipe from the parent; the same pipe stays open so the
/// VM shuts down if the parent disappears.
fn become_vm(log_level: Option<LogLevel>) -> Result<std::convert::Infallible, Error> {
    let spec = VmSpec::read_from_parent()?;
    // Re-validated on this side too: the spec crossed a process boundary, and
    // boot() must never be reachable with one that was not checked here.
    spec.validate()?;

    let lib = Libkrun::load()?;
    if let Some(level) = log_level {
        // Best effort: logging can only be initialised once per process.
        let _ = lib.set_log_level(level);
    }

    spec.boot(&lib)
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

    // Needed only for --supervised, so a missing one is a warning.
    match piebox::guest_binary() {
        Ok(path) => println!("supervisor   {}", path.display()),
        Err(err) => println!("supervisor   {err}"),
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
