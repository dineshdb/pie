//! piebox — a libkrun-backed Linux microVM devbox that hosts pie and, inside
//! it, pie's own sandboxes.
//!
//! The library loads libkrun at runtime, reports what the host supports, builds
//! VM configuration contexts, and boots guests ([`VmSpec`]).
//!
//! One constraint shapes everything: `krun_start_enter` never returns. It takes
//! over the calling process and `exit()`s with the workload's status, so a
//! process that boots a guest can do nothing else. The `piebox` binary
//! therefore re-execs itself as a hidden `__vmm` subcommand whose only job is
//! to become the VM, while the parent supervises it.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod channel;
mod error;
mod ffi;
mod mount;
mod rootfs;
mod supervisor;
mod vm;

pub use channel::{EXIT_PARENT_GONE, SPEC_FD, attach_spec_fd, send_spec};
pub use error::{Error, Result};
pub use mount::{Mount, MountPoints, check_collisions as check_mount_collisions, tagged};
pub use rootfs::{ContainerStorage, container_rootfs};
pub use supervisor::{
    DEFAULT_REPLY_TIMEOUT, ENV_GUEST_BIN, Endpoint, GUEST_BINARY_PREFIX, GUEST_STAGING_DIR,
    StagedSupervisor, Supervisor, guest_binary, guest_target_triple, stage_guest_binary,
};
pub use vm::{
    EXIT_EXEC_FAILED, EXIT_EXEC_NOT_FOUND, EXIT_INIT_SETUP_FAILED, VmSpec, Vsock, loader_env,
};

// The wire types are part of piebox's surface: a caller driving the guest
// supervisor needs them, and re-exporting saves depending on the proto crate.
pub use piebox_proto::{Exit, Request, SUPERVISOR_PORT};

use ffi::Krun;
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};

/// Overrides libkrun discovery with an explicit library path.
pub const ENV_LIBKRUN: &str = "PIEBOX_LIBKRUN";

/// Smallest guest RAM that boots a Linux userland with room to work.
const MIN_RAM_MIB: u32 = 128;

/// Sanity ceiling on guest RAM (1 TiB). libkrun accepts `u32::MAX` MiB without
/// complaint and only fails much later at boot, so the type refuses it here.
const MAX_RAM_MIB: u32 = 1024 * 1024;

/// Number of guest vCPUs.
///
/// `try_from` on deserialization, so a spec arriving from another process is
/// checked exactly like one built in code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "u8")]
pub struct Vcpus(NonZeroU8);

impl Vcpus {
    pub fn new(count: u8) -> Result<Self> {
        NonZeroU8::new(count)
            .map(Self)
            .ok_or_else(|| Error::invalid("vcpus", "must be at least 1"))
    }

    pub const fn get(self) -> u8 {
        self.0.get()
    }
}

/// Guest RAM, in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "u32")]
pub struct RamMib(u32);

impl RamMib {
    pub fn new(mib: u32) -> Result<Self> {
        if !(MIN_RAM_MIB..=MAX_RAM_MIB).contains(&mib) {
            return Err(Error::invalid(
                "ram_mib",
                format!("must be {MIN_RAM_MIB}..={MAX_RAM_MIB} MiB, got {mib}"),
            ));
        }
        Ok(Self(mib))
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u8> for Vcpus {
    type Error = Error;
    fn try_from(count: u8) -> Result<Self> {
        Self::new(count)
    }
}

impl TryFrom<u32> for RamMib {
    type Error = Error;
    fn try_from(mib: u32) -> Result<Self> {
        Self::new(mib)
    }
}

/// libkrun log verbosity (`KRUN_LOG_LEVEL_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    const fn as_raw(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Error => 1,
            Self::Warn => 2,
            Self::Info => 3,
            Self::Debug => 4,
            Self::Trace => 5,
        }
    }
}

/// Build-time libkrun capabilities (`KRUN_FEATURE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Net,
    Blk,
    Gpu,
    Snd,
    Input,
    Efi,
    Tee,
    AmdSev,
    IntelTdx,
    AwsNitro,
    VirglResourceMap2,
    InitBlob,
}

impl Feature {
    /// Every feature, for reporting.
    pub const ALL: [Self; 12] = [
        Self::Net,
        Self::Blk,
        Self::Gpu,
        Self::Snd,
        Self::Input,
        Self::Efi,
        Self::Tee,
        Self::AmdSev,
        Self::IntelTdx,
        Self::AwsNitro,
        Self::VirglResourceMap2,
        Self::InitBlob,
    ];

    const fn as_raw(self) -> u64 {
        match self {
            Self::Net => 0,
            Self::Blk => 1,
            Self::Gpu => 2,
            Self::Snd => 3,
            Self::Input => 4,
            Self::Efi => 5,
            Self::Tee => 6,
            Self::AmdSev => 7,
            Self::IntelTdx => 8,
            Self::AwsNitro => 9,
            Self::VirglResourceMap2 => 10,
            Self::InitBlob => 11,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Net => "net",
            Self::Blk => "blk",
            Self::Gpu => "gpu",
            Self::Snd => "snd",
            Self::Input => "input",
            Self::Efi => "efi",
            Self::Tee => "tee",
            Self::AmdSev => "amd-sev",
            Self::IntelTdx => "intel-tdx",
            Self::AwsNitro => "aws-nitro",
            Self::VirglResourceMap2 => "virgl-resource-map2",
            Self::InitBlob => "init-blob",
        }
    }
}

/// A loaded libkrun.
pub struct Libkrun {
    krun: Krun,
    path: PathBuf,
}

impl std::fmt::Debug for Libkrun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Libkrun").field("path", &self.path).finish()
    }
}

impl Libkrun {
    /// Loads libkrun from `PIEBOX_LIBKRUN`, else the usual install prefixes.
    pub fn load() -> Result<Self> {
        let env_override = std::env::var(ENV_LIBKRUN).ok();
        let homebrew_prefix = std::env::var("HOMEBREW_PREFIX").ok();
        let candidates = ffi::candidates(env_override.as_deref(), homebrew_prefix.as_deref());
        let (krun, path) = Krun::load_any(&candidates)?;
        Ok(Self { krun, path })
    }

    /// Loads libkrun from an explicit path.
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        Ok(Self {
            krun: Krun::load_from(path)?,
            path: path.to_path_buf(),
        })
    }

    /// Path the library was loaded from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolved libkrun entry points.
    pub(crate) const fn krun(&self) -> &Krun {
        &self.krun
    }

    /// Sets libkrun's log verbosity. Only the first call in a process takes
    /// effect; later calls report an error instead of being forwarded.
    ///
    /// `krun_set_log_level` initialises `env_logger`, which panics if a logger
    /// is already installed. That panic cannot unwind out of `extern "C"`, so a
    /// second call would abort the process. The logger is global to the loaded
    /// image and survives `dlclose`, so the guard has to be process-global
    /// rather than per-`Libkrun`.
    pub fn set_log_level(&self, level: LogLevel) -> Result<()> {
        static INIT: std::sync::Once = std::sync::Once::new();
        let mut result = Err(Error::invalid(
            "log_level",
            "libkrun logging is already initialised; it can only be set once per process",
        ));
        INIT.call_once(|| {
            // SAFETY: takes a plain u32; no pointers involved.
            let code = unsafe { (self.krun.set_log_level)(level.as_raw()) };
            result = Error::check("krun_set_log_level", code).map(drop);
        });
        result
    }

    /// Maximum vCPUs the hypervisor accepts, saturated to what `krun_set_vm_config` can carry.
    pub fn max_vcpus(&self) -> Result<u8> {
        // SAFETY: no arguments.
        let code = unsafe { (self.krun.get_max_vcpus)() };
        let max = Error::check("krun_get_max_vcpus", code)?;
        Ok(u8::try_from(max).unwrap_or(u8::MAX))
    }

    /// Whether this libkrun build supports `feature`.
    ///
    /// Infallible by construction: `Feature` only names constants libkrun
    /// defines, so the sole negative answer is `-EINVAL` from a libkrun older
    /// than the constant — which means "not supported".
    pub fn has_feature(&self, feature: Feature) -> bool {
        // SAFETY: takes a plain u64 constant.
        let code = unsafe { (self.krun.has_feature)(feature.as_raw()) };
        code > 0
    }

    /// Creates a VM configuration context, freed on drop.
    pub fn create_ctx(&self) -> Result<Ctx<'_>> {
        // SAFETY: no arguments; returns an id or a negative errno.
        let code = unsafe { (self.krun.create_ctx)() };
        let id = Error::check("krun_create_ctx", code)?;
        Ok(Ctx {
            lib: self,
            id: id as u32,
        })
    }
}

/// A libkrun configuration context.
///
/// Dropping it calls `krun_free_ctx`, so a half-built configuration never leaks.
pub struct Ctx<'lib> {
    lib: &'lib Libkrun,
    id: u32,
}

impl<'lib> Ctx<'lib> {
    /// Raw libkrun context id. Crate-internal: libkrun's context map is
    /// per-process, so this id means nothing to any other process.
    pub(crate) const fn raw_id(&self) -> u32 {
        self.id
    }

    /// The library this context belongs to.
    pub(crate) const fn lib(&self) -> &'lib Libkrun {
        self.lib
    }

    /// Sets vCPU count and RAM.
    ///
    /// The vCPU count is checked against the hypervisor limit first, so an
    /// oversized request reports the real limit instead of a bare `EINVAL`.
    pub fn set_vm_config(&mut self, vcpus: Vcpus, ram: RamMib) -> Result<()> {
        let max = self.lib.max_vcpus()?;
        if vcpus.get() > max {
            return Err(Error::invalid(
                "vcpus",
                format!("{} requested, hypervisor allows {max}", vcpus.get()),
            ));
        }
        // SAFETY: scalar arguments only; `id` came from krun_create_ctx.
        let code = unsafe { (self.lib.krun.set_vm_config)(self.id, vcpus.get(), ram.get()) };
        Error::check("krun_set_vm_config", code).map(drop)
    }
}

impl Drop for Ctx<'_> {
    fn drop(&mut self) {
        // SAFETY: `id` is a live context created by this library.
        let code = unsafe { (self.lib.krun.free_ctx)(self.id) };
        if code < 0 {
            tracing::warn!(ctx = self.id, code, "krun_free_ctx failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set this to make a failure to load libkrun a test failure rather than a skip.
    const ENV_REQUIRE: &str = "PIEBOX_REQUIRE_LIBKRUN";

    /// Loads libkrun, or returns `None` when the host genuinely has none (CI).
    ///
    /// A libkrun that exists but will not load (wrong arch, missing
    /// dependency, typo'd `PIEBOX_LIBKRUN`) is a failure, not a skip —
    /// otherwise the suite reports green on a host that should have worked.
    fn libkrun_or_skip() -> Option<Libkrun> {
        match Libkrun::load() {
            Ok(lib) => Some(lib),
            Err(err) => {
                if std::env::var_os(ENV_REQUIRE).is_some() {
                    panic!("{ENV_REQUIRE} is set but libkrun could not be loaded: {err}");
                }
                if let Error::LibraryLoad { candidates, .. } = &err
                    && let Some(present) = candidates.iter().find(|p| p.exists())
                {
                    panic!(
                        "libkrun exists at {} but failed to load: {err}",
                        present.display()
                    );
                }
                eprintln!("skipping: libkrun not installed on this host ({err})");
                None
            }
        }
    }

    #[test]
    fn vcpus_rejects_zero() {
        assert!(Vcpus::new(0).is_err());
        assert_eq!(Vcpus::new(2).unwrap().get(), 2);
    }

    #[test]
    fn ram_rejects_values_outside_the_supported_range() {
        assert!(RamMib::new(0).is_err());
        assert!(RamMib::new(MIN_RAM_MIB - 1).is_err());
        // libkrun accepts u32::MAX MiB and fails only at boot; we refuse early.
        assert!(RamMib::new(u32::MAX).is_err());
        assert!(RamMib::new(MAX_RAM_MIB + 1).is_err());
        assert_eq!(RamMib::new(2048).unwrap().get(), 2048);
        assert_eq!(RamMib::new(MAX_RAM_MIB).unwrap().get(), MAX_RAM_MIB);
    }

    #[test]
    fn newtypes_convert_with_try_into() {
        let vcpus: Vcpus = 4u8.try_into().expect("4 vcpus");
        assert_eq!(vcpus.get(), 4);
        let ram: RamMib = 4096u32.try_into().expect("4096 MiB");
        assert_eq!(ram.get(), 4096);
        assert!(Vcpus::try_from(0u8).is_err());
    }

    #[test]
    fn feature_names_are_unique() {
        let mut names: Vec<_> = Feature::ALL.iter().map(|f| f.name()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    /// Absolute values, transcribed from `KRUN_FEATURE_*` in libkrun.h. Pinning
    /// them catches a reorder of `ALL` and `as_raw` together, which an
    /// index-based check would not.
    #[test]
    fn feature_raw_values_match_header() {
        assert_eq!(Feature::Net.as_raw(), 0);
        assert_eq!(Feature::Blk.as_raw(), 1);
        assert_eq!(Feature::Gpu.as_raw(), 2);
        assert_eq!(Feature::Snd.as_raw(), 3);
        assert_eq!(Feature::Input.as_raw(), 4);
        assert_eq!(Feature::Efi.as_raw(), 5);
        assert_eq!(Feature::Tee.as_raw(), 6);
        assert_eq!(Feature::AmdSev.as_raw(), 7);
        assert_eq!(Feature::IntelTdx.as_raw(), 8);
        assert_eq!(Feature::AwsNitro.as_raw(), 9);
        assert_eq!(Feature::VirglResourceMap2.as_raw(), 10);
        assert_eq!(Feature::InitBlob.as_raw(), 11);
    }

    /// Absolute values, transcribed from `KRUN_LOG_LEVEL_*` in libkrun.h.
    #[test]
    fn log_level_raw_values_match_header() {
        assert_eq!(LogLevel::Off.as_raw(), 0);
        assert_eq!(LogLevel::Error.as_raw(), 1);
        assert_eq!(LogLevel::Warn.as_raw(), 2);
        assert_eq!(LogLevel::Info.as_raw(), 3);
        assert_eq!(LogLevel::Debug.as_raw(), 4);
        assert_eq!(LogLevel::Trace.as_raw(), 5);
    }

    #[test]
    fn host_reports_capabilities() {
        let Some(lib) = libkrun_or_skip() else { return };
        assert!(lib.path().exists(), "{}", lib.path().display());
        let max = lib.max_vcpus().expect("max_vcpus");
        assert!(max >= 1, "max_vcpus = {max}");
        // Every libkrun build piebox can use has virtio-blk.
        assert!(lib.has_feature(Feature::Blk));
    }

    #[test]
    fn ctx_round_trip() {
        let Some(lib) = libkrun_or_skip() else { return };
        let mut ctx = lib.create_ctx().expect("create_ctx");
        ctx.set_vm_config(Vcpus::new(1).unwrap(), RamMib::new(512).unwrap())
            .expect("set_vm_config");
        drop(ctx); // frees the context; a leak would show up as an error log
    }

    /// libkrun itself accepts any vCPU count at config time (255 returns 0) and
    /// only fails later at boot, so this pre-check is the real guard. The bound
    /// is derived from the host: KVM reports far more vCPUs than HVF does.
    #[test]
    fn ctx_rejects_more_vcpus_than_the_host_allows() {
        let Some(lib) = libkrun_or_skip() else { return };
        let max = lib.max_vcpus().expect("max_vcpus");
        let Some(too_many) = max.checked_add(1) else {
            eprintln!("skipping: host allows the full u8 vCPU range");
            return;
        };
        let mut ctx = lib.create_ctx().expect("create_ctx");
        let err = ctx
            .set_vm_config(Vcpus::new(too_many).unwrap(), RamMib::new(512).unwrap())
            .expect_err("over-limit vCPU count must be rejected");
        assert!(
            matches!(err, Error::InvalidValue { what: "vcpus", .. }),
            "{err:?}"
        );
    }

    #[test]
    fn log_level_is_settable_once_then_refused() {
        let Some(lib) = libkrun_or_skip() else { return };
        // First call wins; a second must be refused rather than abort the
        // process (env_logger panics across the FFI boundary).
        lib.set_log_level(LogLevel::Off).expect("first call");
        let err = lib.set_log_level(LogLevel::Debug).expect_err("second call");
        assert!(
            matches!(
                err,
                Error::InvalidValue {
                    what: "log_level",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn missing_library_is_reported_clearly() {
        let err = Libkrun::load_from("/nonexistent/libkrun.dylib").expect_err("must fail");
        assert!(matches!(err, Error::LibraryLoad { .. }), "{err:?}");
        assert!(err.to_string().contains("PIEBOX_LIBKRUN"), "{err}");
    }
}
