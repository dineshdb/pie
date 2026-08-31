//! Booting a guest.
//!
//! [`VmSpec`] is the whole description of a VM, and it exists as a separate,
//! copyable value for one reason: `krun_start_enter` never returns. The VMM
//! takes over the calling process and `exit()`s with the workload's status, so
//! the process that boots a guest can do nothing else — no supervision, no
//! egress proxy, no control channel. piebox therefore re-execs itself as a
//! child whose only job is to become the VM, and the spec is what crosses that
//! boundary.
//!
//! A libkrun context cannot cross it: context ids live in a per-process global
//! map inside libkrun, so [`VmSpec::boot`] runs in the child and builds its own.

use crate::error::{Error, Result};
use crate::{Ctx, Libkrun, RamMib, Vcpus};
use std::ffi::{CString, c_char};
use std::path::{Path, PathBuf};

/// Most arguments a guest workload can receive.
///
/// The kernel caps init's argv at `CONFIG_INIT_ENV_ARG_LIMIT` (32); one more
/// makes it panic during boot. Measured, not guessed: 32 works, 33 runs nothing.
const MAX_GUEST_ARGS: usize = 32;

/// Most environment variables a guest workload can receive. Lower than the
/// kernel's 32 because libkrun spends some of that budget on `KRUN_INIT`,
/// `KRUN_WORKDIR` and friends.
const MAX_GUEST_ENV: usize = 24;

/// Bytes of arguments and environment that fit on the command line.
///
/// `COMMAND_LINE_SIZE` is 2048 on aarch64, and libkrun's own prolog plus its
/// `KRUN_*` entries take a few hundred. Overshooting aborts the process from
/// inside libkrun, so the margin is deliberately generous.
const MAX_CMDLINE_BUDGET: usize = 2048 - 512;

/// Exit codes libkrun's built-in init reserves for its own failures.
///
/// They collide with ordinary workload exit codes, so they are reported rather
/// than passed through silently.
pub const EXIT_INIT_SETUP_FAILED: i32 = 125;
pub const EXIT_EXEC_FAILED: i32 = 126;
pub const EXIT_EXEC_NOT_FOUND: i32 = 127;

/// Everything needed to boot one guest.
#[derive(Debug, Clone)]
pub struct VmSpec {
    /// Host directory exposed as the guest's root filesystem (virtio-fs).
    pub rootfs: PathBuf,
    pub vcpus: Vcpus,
    pub ram: RamMib,
    /// Working directory *inside* the guest.
    pub workdir: Option<PathBuf>,
    /// Executable to run, as an absolute path inside the guest.
    pub exec: PathBuf,
    /// Arguments passed to the executable, excluding its own name.
    pub args: Vec<String>,
    /// Environment for the workload.
    ///
    /// Always set explicitly: passing NULL makes libkrun copy the host's
    /// environment into the guest, which leaks host paths and secrets into a
    /// place that is supposed to be isolated.
    pub env: Vec<(String, String)>,
}

impl VmSpec {
    /// A spec with sensible defaults for running one command in a guest.
    pub fn new(rootfs: impl Into<PathBuf>, exec: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            rootfs: rootfs.into(),
            vcpus: Vcpus::new(2)?,
            ram: RamMib::new(1024)?,
            workdir: None,
            exec: exec.into(),
            args: Vec::new(),
            env: default_env(),
        })
    }

    /// Boots the guest. **Does not return on success**: libkrun takes over the
    /// process and exits with the workload's status.
    ///
    /// # Errors
    /// Returns an error only when the VM could not be configured or started,
    /// which is the sole case where control comes back.
    pub fn boot(&self, lib: &Libkrun) -> Result<std::convert::Infallible> {
        self.validate()?;

        let mut ctx = lib.create_ctx()?;
        ctx.set_vm_config(self.vcpus, self.ram)?;
        self.apply_root(&ctx)?;
        self.apply_workdir(&ctx)?;
        self.apply_exec(&ctx)?;

        tracing::debug!(
            rootfs = %self.rootfs.display(),
            exec = %self.exec.display(),
            vcpus = self.vcpus.get(),
            ram_mib = self.ram.get(),
            "entering microVM",
        );

        // SAFETY: the context is fully configured; on success this call never
        // returns, so `ctx`'s Drop is unreachable and that is fine.
        let code = unsafe { (lib.krun().start_enter)(ctx.raw_id()) };
        // Reaching this line at all means the VM did not start.
        Err(Error::Krun {
            call: "krun_start_enter",
            code,
        })
    }

    /// Checks the spec without touching libkrun, so bad input can be refused
    /// before a hypervisor is involved (or on a host that has none).
    ///
    /// # Errors
    /// Reports the first field that a guest could not be booted with.
    pub fn validate(&self) -> Result<()> {
        if !self.rootfs.is_dir() {
            return Err(Error::invalid(
                "rootfs",
                format!("{} is not a directory", self.rootfs.display()),
            ));
        }
        if !self.exec.is_absolute() {
            return Err(Error::invalid(
                "exec",
                format!("{} must be absolute inside the guest", self.exec.display()),
            ));
        }
        self.validate_cmdline()
    }

    /// Checks what a kernel command line can actually carry.
    ///
    /// libkrun does not pass the workload's arguments and environment through
    /// a syscall: it appends them to the guest's kernel command line, wrapping
    /// each token in double quotes *without escaping anything*. Everything
    /// below is therefore a real limit of the transport, and exceeding one
    /// fails in a way that looks like success:
    ///
    /// - too many arguments and the guest kernel panics, reboots (`panic=-1`)
    ///   and the VMM exits 0 having run nothing at all;
    /// - a `"` closes libkrun's quoting early, so the remaining tokens are
    ///   re-read as kernel parameters — an environment value containing
    ///   `" init=/bin/echo "` replaces the workload entirely;
    /// - an over-long line makes `krun_start_enter` panic across the FFI
    ///   boundary, which aborts the process rather than returning an error.
    fn validate_cmdline(&self) -> Result<()> {
        if self.args.len() > MAX_GUEST_ARGS {
            return Err(Error::invalid(
                "args",
                format!(
                    "{} arguments, but the guest kernel command line carries at most {MAX_GUEST_ARGS}",
                    self.args.len()
                ),
            ));
        }
        if self.env.len() > MAX_GUEST_ENV {
            return Err(Error::invalid(
                "env",
                format!(
                    "{} variables, but the guest kernel command line carries at most {MAX_GUEST_ENV}",
                    self.env.len()
                ),
            ));
        }

        // `exec` and `workdir` are interpolated *unquoted* into KRUN_INIT and
        // KRUN_WORKDIR, so whitespace splits them into separate parameters.
        no_quotes_or_control(path_str(&self.exec)?, "exec")?;
        if !path_str(&self.exec)?.split_whitespace().count().eq(&1) {
            return Err(Error::invalid("exec", "must not contain whitespace"));
        }
        if let Some(workdir) = &self.workdir {
            let workdir = path_str(workdir)?;
            no_quotes_or_control(workdir, "workdir")?;
            if workdir.split_whitespace().count() != 1 {
                return Err(Error::invalid("workdir", "must not contain whitespace"));
            }
        }

        for arg in &self.args {
            no_quotes_or_control(arg, "argument")?;
            // clap consumes a `--` token wherever it appears, so this argument
            // would be dropped silently on its way to the child VMM process.
            // TODO(stage 3): forward the spec over a pipe instead of argv and
            // this restriction, along with the secrecy problem below, goes away.
            if arg == "--" {
                return Err(Error::invalid(
                    "argument",
                    "a literal `--` cannot be forwarded to the guest yet",
                ));
            }
        }
        for (key, value) in &self.env {
            if key.is_empty() {
                return Err(Error::invalid("environment variable", "name is empty"));
            }
            no_quotes_or_control(key, "environment variable name")?;
            no_quotes_or_control(value, "environment variable value")?;
            // A `=` would split the assignment; a `.` makes the kernel treat
            // the token as a module parameter and drop it.
            if key.contains('=') || key.contains('.') || key.contains(char::is_whitespace) {
                return Err(Error::invalid(
                    "environment variable name",
                    format!("{key:?} must not contain '=', '.' or whitespace"),
                ));
            }
        }

        let used = self.cmdline_bytes()?;
        if used > MAX_CMDLINE_BUDGET {
            return Err(Error::invalid(
                "command line",
                format!(
                    "{used} bytes of arguments and environment, but only {MAX_CMDLINE_BUDGET} fit \
                     on the guest kernel command line"
                ),
            ));
        }
        Ok(())
    }

    /// Bytes this spec contributes to the guest kernel command line.
    fn cmdline_bytes(&self) -> Result<usize> {
        // Each token costs its length plus the quotes and separating space
        // libkrun adds around it.
        let quoted = |len: usize| len + 3;
        let mut total = path_str(&self.exec)?.len();
        if let Some(workdir) = &self.workdir {
            total += path_str(workdir)?.len();
        }
        total += self.args.iter().map(|a| quoted(a.len())).sum::<usize>();
        total += self
            .env
            .iter()
            .map(|(k, v)| quoted(k.len() + v.len() + 1))
            .sum::<usize>();
        Ok(total)
    }

    fn apply_root(&self, ctx: &Ctx<'_>) -> Result<()> {
        let root = c_path(&self.rootfs, "rootfs")?;
        // SAFETY: `root` outlives the call; libkrun copies the string.
        let code = unsafe { (ctx.lib().krun().set_root)(ctx.raw_id(), root.as_ptr()) };
        Error::check("krun_set_root", code).map(drop)
    }

    fn apply_workdir(&self, ctx: &Ctx<'_>) -> Result<()> {
        let Some(workdir) = &self.workdir else {
            return Ok(());
        };
        let workdir = c_path(workdir, "workdir")?;
        // SAFETY: as above.
        let code = unsafe { (ctx.lib().krun().set_workdir)(ctx.raw_id(), workdir.as_ptr()) };
        Error::check("krun_set_workdir", code).map(drop)
    }

    fn apply_exec(&self, ctx: &Ctx<'_>) -> Result<()> {
        let exec = c_path(&self.exec, "exec")?;

        // The owned CStrings must outlive the call: a pointer array built from
        // temporaries would dangle before libkrun ever read it.
        let args: Vec<CString> = self
            .args
            .iter()
            .map(|arg| c_str(arg, "argument"))
            .collect::<Result<_>>()?;
        let env: Vec<CString> = self
            .env
            .iter()
            .map(|(key, value)| c_str(&format!("{key}={value}"), "environment variable"))
            .collect::<Result<_>>()?;

        let argv = null_terminated(&args);
        let envp = null_terminated(&env);

        // SAFETY: both arrays are NULL-terminated as libkrun requires, and
        // every pointer stays valid until this call returns.
        let code = unsafe {
            (ctx.lib().krun().set_exec)(ctx.raw_id(), exec.as_ptr(), argv.as_ptr(), envp.as_ptr())
        };
        Error::check("krun_set_exec", code).map(drop)
    }
}

/// Rejects what libkrun's unescaped quoting cannot carry.
fn no_quotes_or_control(value: &str, what: &'static str) -> Result<()> {
    if let Some(bad) = value.chars().find(|c| *c == '"' || c.is_control()) {
        return Err(Error::invalid(
            what,
            format!(
                "{value:?} contains {bad:?}, which libkrun cannot encode on the guest kernel \
                 command line"
            ),
        ));
    }
    Ok(())
}

/// A path as UTF-8, which is what libkrun requires of every string it takes.
fn path_str(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        Error::invalid(
            "path",
            format!(
                "{} is not valid UTF-8, which libkrun requires",
                path.display()
            ),
        )
    })
}

/// Loader search-path variable for the host platform.
const LOADER_PATH_VAR: &str = if cfg!(target_os = "macos") {
    "DYLD_FALLBACK_LIBRARY_PATH"
} else {
    "LD_LIBRARY_PATH"
};

/// dyld's own defaults, which setting the variable would otherwise replace.
const LOADER_PATH_DEFAULT: &str = "/usr/local/lib:/usr/lib";

/// Environment a child VMM process needs before it can boot a guest.
///
/// libkrun does *not* link libkrunfw (the kernel payload); it `dlopen`s it by
/// leaf name during `krun_start_enter`. Homebrew's prefix is not on dyld's
/// search path, so the VM dies with "Couldn't find or load libkrunfw.5.dylib"
/// unless the loader is told where to look. dyld reads these paths once at
/// process start, so this has to be set by the *parent* when spawning the
/// child — setting it from inside the child is too late.
///
/// Also pins the child to the exact libkrun the parent validated, so the two
/// processes cannot end up using different builds.
pub fn loader_env(libkrun: &Path) -> Vec<(&'static str, std::ffi::OsString)> {
    loader_env_with(libkrun, std::env::var_os(LOADER_PATH_VAR))
}

fn loader_env_with(
    libkrun: &Path,
    existing: Option<std::ffi::OsString>,
) -> Vec<(&'static str, std::ffi::OsString)> {
    let mut env = vec![(crate::ENV_LIBKRUN, libkrun.as_os_str().to_os_string())];

    if let Some(dir) = libkrun.parent() {
        let mut path = dir.as_os_str().to_os_string();
        path.push(":");
        match existing.filter(|value| !value.is_empty()) {
            Some(existing) => path.push(existing),
            None => path.push(LOADER_PATH_DEFAULT),
        }
        env.push((LOADER_PATH_VAR, path));
    }
    env
}

/// Environment given to a guest workload when the caller supplies none.
fn default_env() -> Vec<(String, String)> {
    [
        (
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        ),
        ("HOME", "/root"),
        ("TERM", "xterm-256color"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Builds the NULL-terminated pointer array libkrun expects.
fn null_terminated(values: &[CString]) -> Vec<*const c_char> {
    let mut pointers: Vec<*const c_char> = values.iter().map(|v| v.as_ptr()).collect();
    pointers.push(std::ptr::null());
    pointers
}

/// Converts a path for FFI, treating an interior NUL as an error.
///
/// Truncating at a NUL instead of failing is how a path silently becomes a
/// different path. Non-UTF-8 is rejected here rather than deferred, because
/// libkrun calls `CStr::to_str()` on everything and would answer with a bare
/// `-EINVAL` that names nothing.
fn c_path(path: &Path, what: &'static str) -> Result<CString> {
    CString::new(path_str(path)?)
        .map_err(|_| Error::invalid(what, format!("{} contains a NUL byte", path.display())))
}

fn c_str(value: &str, what: &'static str) -> Result<CString> {
    CString::new(value).map_err(|_| Error::invalid(what, format!("{value:?} contains a NUL byte")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> VmSpec {
        VmSpec::new("/", "/bin/true").expect("spec")
    }

    #[test]
    fn default_environment_is_explicit_and_excludes_the_host() {
        let spec = spec();
        let keys: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"PATH"), "{keys:?}");
        // Whatever unusual variables this host has must not appear.
        assert!(spec.env.len() <= 4, "{:?}", spec.env);
    }

    #[test]
    fn relative_exec_is_rejected() {
        let mut spec = spec();
        spec.exec = PathBuf::from("bin/true");
        let err = spec.validate().expect_err("relative exec must be rejected");
        assert!(
            matches!(err, Error::InvalidValue { what: "exec", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn missing_rootfs_is_rejected() {
        let mut spec = spec();
        spec.rootfs = PathBuf::from("/nonexistent/piebox-rootfs");
        let err = spec
            .validate()
            .expect_err("missing rootfs must be rejected");
        assert!(
            matches!(err, Error::InvalidValue { what: "rootfs", .. }),
            "{err:?}"
        );
    }

    /// Every case below used to look like success while running nothing, or
    /// while running something other than what was asked for.
    #[test]
    fn a_spec_the_kernel_command_line_cannot_carry_is_rejected() {
        /// Field expected to be blamed, and how to break it.
        type Case = (&'static str, Box<dyn Fn(&mut VmSpec)>);

        let cases: Vec<Case> = vec![
            (
                "args",
                Box::new(|s: &mut VmSpec| {
                    s.args = (0..MAX_GUEST_ARGS + 1).map(|i| i.to_string()).collect();
                }),
            ),
            (
                "env",
                Box::new(|s: &mut VmSpec| {
                    s.env = (0..MAX_GUEST_ENV + 1)
                        .map(|i| (format!("K{i}"), "v".to_string()))
                        .collect();
                }),
            ),
            (
                "argument",
                Box::new(|s: &mut VmSpec| s.args = vec!["a\"b".to_string()]),
            ),
            (
                "argument",
                Box::new(|s: &mut VmSpec| s.args = vec!["--".to_string()]),
            ),
            (
                "argument",
                Box::new(|s: &mut VmSpec| s.args = vec!["a\nb".to_string()]),
            ),
            (
                "environment variable value",
                Box::new(|s: &mut VmSpec| {
                    s.env = vec![("A".to_string(), "1\" init=/bin/echo \"".to_string())];
                }),
            ),
            (
                "environment variable name",
                Box::new(|s: &mut VmSpec| {
                    s.env = vec![("a.b".to_string(), "v".to_string())];
                }),
            ),
            (
                "environment variable",
                Box::new(|s: &mut VmSpec| s.env = vec![(String::new(), "v".to_string())]),
            ),
            (
                "command line",
                Box::new(|s: &mut VmSpec| s.args = vec!["x".repeat(MAX_CMDLINE_BUDGET + 1)]),
            ),
            (
                "workdir",
                Box::new(|s: &mut VmSpec| s.workdir = Some(PathBuf::from("/tmp/a b"))),
            ),
            (
                "exec",
                Box::new(|s: &mut VmSpec| s.exec = PathBuf::from("/bin/a b")),
            ),
        ];

        for (what, mutate) in cases {
            let mut spec = spec();
            mutate(&mut spec);
            let err = spec
                .validate_cmdline()
                .expect_err(&format!("{what} case must be rejected"));
            match err {
                Error::InvalidValue { what: got, .. } => {
                    assert_eq!(got, what, "wrong field blamed");
                }
                other => panic!("expected an InvalidValue for {what}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_spec_within_the_limits_is_accepted() {
        let mut spec = spec();
        spec.args = (0..MAX_GUEST_ARGS).map(|i| i.to_string()).collect();
        spec.workdir = Some(PathBuf::from("/tmp"));
        spec.env.push(("FOO".to_string(), "bar".to_string()));
        spec.validate_cmdline().expect("within limits");
    }

    #[test]
    fn non_utf8_paths_are_named_rather_than_left_to_libkrun() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff, b'x']));
        let err = c_path(&path, "rootfs").expect_err("non-UTF-8 must be rejected");
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn nul_bytes_are_errors_not_truncations() {
        assert!(c_path(Path::new("/tmp/a\0b"), "rootfs").is_err());
        assert!(c_str("a\0b", "argument").is_err());
        assert!(c_path(Path::new("/tmp/ok"), "rootfs").is_ok());
    }

    #[test]
    fn loader_env_points_at_the_libkrun_directory_and_pins_the_library() {
        let env = loader_env_with(Path::new("/opt/homebrew/lib/libkrun.1.dylib"), None);
        let libkrun = env
            .iter()
            .find(|(k, _)| *k == crate::ENV_LIBKRUN)
            .expect("libkrun pin");
        assert_eq!(libkrun.1, "/opt/homebrew/lib/libkrun.1.dylib");

        let (_, path) = env
            .iter()
            .find(|(k, _)| *k == LOADER_PATH_VAR)
            .expect("loader path");
        let path = path.to_string_lossy();
        assert!(path.starts_with("/opt/homebrew/lib:"), "{path}");
        // Replacing the variable must not drop the loader's own defaults.
        assert!(path.contains("/usr/lib"), "{path}");
    }

    #[test]
    fn loader_env_prepends_to_an_existing_search_path() {
        let env = loader_env_with(
            Path::new("/custom/lib/libkrun.1.dylib"),
            Some("/already/here".into()),
        );
        let (_, path) = env
            .iter()
            .find(|(k, _)| *k == LOADER_PATH_VAR)
            .expect("loader path");
        assert_eq!(path.to_string_lossy(), "/custom/lib:/already/here");
    }

    #[test]
    fn pointer_arrays_are_null_terminated() {
        let values = vec![c_str("a", "x").unwrap(), c_str("b", "x").unwrap()];
        let pointers = null_terminated(&values);
        assert_eq!(pointers.len(), 3);
        assert!(pointers.last().unwrap().is_null());

        // An empty list still needs its terminator.
        let pointers = null_terminated(&[]);
        assert_eq!(pointers.len(), 1);
        assert!(pointers[0].is_null());
    }
}
