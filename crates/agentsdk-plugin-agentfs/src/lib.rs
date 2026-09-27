//! Agent-filesystem plugin: the agentsdk fs tool surface (`Read`,
//! `Write`, `Edit`, `Ls`, `Glob`) backed by [`agentfs_sdk`] instead of
//! the host filesystem.
//!
//! The model-facing surface mirrors `agentsdk-plugin-fs` tool for tool
//! (names, inputs, output shapes), so hosts choose per need: register
//! [`AgentFsPlugin`] for tracked runs, `FileSystemPlugin` for direct
//! host runs. The difference is where bytes land and what is
//! remembered:
//!
//! - Writes land on the host immediately (write-through) and are
//!   journalled into the overlay, so the fs tools, the shell, git and
//!   builds share one tree — the host is the source of truth, the
//!   overlay is the audit trail. Reads prefer the host. `sync_to_host`
//!   stays as the end-of-turn safety net that retries any write the
//!   host rejected mid-turn.
//! - Every mutation is recorded as file history in the agentfs kv
//!   facet (`fshist:{session}:{stamp}-{seq}` → [`HistoryRecord`]:
//!   op, path, before/after contents), namespaced per session.
//! - Path policy rides the configured sandbox provider: under
//!   `platform` the [`PlatformSandbox`] checks gate every operation;
//!   under `none` they pass through, leaving the overlay's own
//!   one-directory chroot as the only boundary.
//!
//! One instance per session: each session opens its own agentfs file
//! (no cross-process lock contention, history isolated per session).
//! Files accumulate under the caller's directory — retention/GC is the
//! host's job (TODO: `pie agentfs gc`).

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use agentfs_sdk::{AgentFS, AgentFSOptions, FsError};
use agentsdk::PluginTools;
use agentsdk::core::cwd::Cwd;
use agentsdk::core::plugin::{AgentPlugin, PluginContext, PluginToolCall};
use agentsdk::core::tools::ToolDefinition;
use async_trait::async_trait;
use p1e_sandbox::PlatformSandbox;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

fn lock<T>(guard: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    guard.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wall-clock milliseconds. Far-future overflow saturates instead of
/// wrapping — history keys only need to sort.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// History key prefix: `fshist:{session}:{stamp_ms:013}-{seq:06}`.
/// The stamp sorts by arrival; the per-handle sequence breaks
/// same-millisecond ties (seeded from the existing key count at open,
/// so per-turn handles never collide).
fn history_prefix(session: &str) -> String {
    format!("fshist:{session}:")
}

/// One tracked file mutation: what changed, where, and both sides of
/// the edit. `before` is `None` when the path did not exist.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryRecord {
    pub op: String,
    pub path: String,
    pub before: Option<String>,
    pub after: String,
    pub ts: i64,
}

/// An open per-session agent filesystem: the overlay handle, the host
/// base it mirrors, the session its history is namespaced under, and
/// the path policy every operation is gated by (a pass-through under
/// the `none` provider).
pub struct AgentFsHandle {
    agent: Arc<AgentFS>,
    /// The session file backing this handle (for lifecycle cleanup).
    db_path: PathBuf,
    base: PathBuf,
    session: String,
    policy: Arc<PlatformSandbox>,
    seq: Arc<AtomicU64>,
    /// Whether the file already held history when opened. Only a file
    /// that started empty AND stayed untouched is removed on drop —
    /// otherwise one read-only turn would wipe earlier turns' audit.
    preexisting_history: bool,
    /// History records written by this handle. A journalled mutation
    /// keeps the session file on drop even when its writes already
    /// converged (the audit is the file's whole point).
    recorded: Arc<AtomicU64>,
    /// Absolute host paths still pending host materialization —
    /// write-through removes paths as they land; leftovers are the
    /// end-of-turn sync's retry list.
    touched: Arc<Mutex<Vec<PathBuf>>>,
}

impl Clone for AgentFsHandle {
    fn clone(&self) -> Self {
        Self {
            agent: Arc::clone(&self.agent),
            db_path: self.db_path.clone(),
            base: self.base.clone(),
            session: self.session.clone(),
            policy: Arc::clone(&self.policy),
            seq: Arc::clone(&self.seq),
            preexisting_history: self.preexisting_history,
            recorded: Arc::clone(&self.recorded),
            touched: Arc::clone(&self.touched),
        }
    }
}

impl std::fmt::Debug for AgentFsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentFsHandle")
            .field("db_path", &self.db_path)
            .field("base", &self.base)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// A handle that never wrote to a fresh file removes it on drop, so
/// read-only (or cancelled-before-write) turns leave no empty session
/// files behind. Removal is idempotent across clones and silent —
/// cleanup must never fail a turn. Handles over files with history, or
/// that wrote, are kept: the audit outlives the process.
impl Drop for AgentFsHandle {
    fn drop(&mut self) {
        if self.preexisting_history
            || self.recorded.load(Ordering::Relaxed) > 0
            || !lock(&self.touched).is_empty()
        {
            return;
        }
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let sibling = format!("{}{suffix}", self.db_path.display());
            let _ = std::fs::remove_file(sibling);
        }
    }
}

impl AgentFsHandle {
    /// Open (creating) the session's agentfs file at `db_path`, mirroring
    /// host directory `base`, namespacing history under `session_id`.
    /// `policy` gates every operation; under the `none` provider it
    /// passes everything through, leaving the overlay's base chroot.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be created/opened or the existing
    /// history cannot be listed.
    pub async fn open(
        db_path: &Path,
        base: &Path,
        session_id: &str,
        policy: PlatformSandbox,
    ) -> Result<Self, String> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("agentfs dir: {e}"))?;
        }
        let agent = AgentFS::open(AgentFSOptions {
            path: Some(db_path.display().to_string()),
            ..AgentFSOptions::default()
        })
        .await
        .map_err(|e| format!("agentfs open {}: {e}", db_path.display()))?;
        let agent = Arc::new(agent);
        // Seed the sequence past existing records so a fresh handle over
        // an old file never reuses a key.
        let prefix = history_prefix(session_id);
        let existing = agent
            .kv
            .keys()
            .await
            .map_err(|e| format!("agentfs history list: {e}"))?
            .into_iter()
            .filter(|key| key.starts_with(&prefix))
            .count();
        Ok(Self {
            agent,
            db_path: db_path.to_path_buf(),
            base: base.to_path_buf(),
            session: session_id.to_string(),
            policy: Arc::new(policy),
            seq: Arc::new(AtomicU64::new(existing as u64)),
            preexisting_history: existing > 0,
            recorded: Arc::new(AtomicU64::new(0)),
            touched: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// The session this handle tracks history for.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The session file backing this handle.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Map a resolved absolute host path onto its overlay path (`/rel`
    /// form). Paths outside the base are refused — the overlay mirrors
    /// one directory, like a chroot. The path is normalized lexically
    /// first, so `..` cannot slip an outside path into the overlay root.
    fn overlay_path(&self, abs: &Path) -> Result<String, String> {
        let mut clean = PathBuf::new();
        for component in abs.components() {
            use std::path::Component;
            match component {
                Component::ParentDir => {
                    if !clean.pop() {
                        return Err(format!("path {} escapes its root", abs.display()));
                    }
                }
                Component::CurDir => {}
                other => clean.push(other.as_os_str()),
            }
        }
        let rel = clean.strip_prefix(&self.base).map_err(|_| {
            format!(
                "path {} is outside the agentfs base {}",
                abs.display(),
                self.base.display()
            )
        })?;
        if rel.as_os_str().is_empty() {
            return Ok("/".to_string());
        }
        Ok(format!("/{}", rel.display()))
    }

    /// Read the live bytes: the host is the source of truth (the shell,
    /// git and builds mutate it), the overlay copy only covers the window
    /// where a write-through failed. `None` when the path exists in
    /// neither.
    async fn read_text(&self, abs: &Path) -> Result<Option<String>, String> {
        match std::fs::read_to_string(abs) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let rel = self.overlay_path(abs)?;
                match self
                    .agent
                    .fs
                    .read_file(&rel)
                    .await
                    .map_err(|e| format!("agentfs read {}: {e}", abs.display()))?
                {
                    Some(bytes) => String::from_utf8(bytes)
                        .map(Some)
                        .map_err(|e| format!("agentfs read {}: not UTF-8: {e}", abs.display())),
                    None => Ok(None),
                }
            }
            Err(e) => Err(format!("host read {}: {e}", abs.display())),
        }
    }

    /// Create overlay parents for `rel` (`/a/b/c` → `/a`, `/a/b`).
    /// Existing directories are kept.
    async fn mkdir_p(&self, rel: &str) -> Result<(), String> {
        let mut prefix = String::new();
        for component in rel.split('/').filter(|c| !c.is_empty()) {
            prefix.push('/');
            prefix.push_str(component);
            // Last component is the file itself.
            if prefix.len() == rel.len() {
                break;
            }
            match self.agent.fs.lstat(&prefix).await {
                Ok(Some(stats)) if stats.is_directory() => {}
                Ok(Some(_)) => {
                    return Err(format!("agentfs mkdir: {prefix} is not a directory"));
                }
                Ok(None) => self
                    .agent
                    .fs
                    .mkdir(&prefix, 0, 0)
                    .await
                    .map_err(|e| format!("agentfs mkdir {prefix}: {e}"))?,
                Err(e) => return Err(format!("agentfs stat {prefix}: {e}")),
            }
        }
        Ok(())
    }

    /// Overwrite `rel` with `bytes`, creating parents. Missing files are
    /// created; existing ones are replaced whole. Every mutation flows
    /// through here: the overlay is journalled first (audit), then the
    /// bytes land on the host immediately — one tree for the fs tools,
    /// the shell, git and builds. On success the path is dropped from
    /// the pending-sync list (already converged); on host failure the
    /// error surfaces to the tool caller and the end-of-turn sync
    /// retries the materialization.
    async fn write_bytes(&self, rel: &str, bytes: &[u8]) -> Result<(), String> {
        self.mkdir_p(rel).await?;
        match self.agent.fs.remove(rel).await {
            Ok(()) | Err(agentfs_sdk::error::Error::Fs(FsError::NotFound)) => {}
            Err(e) => return Err(format!("agentfs remove {rel}: {e}")),
        }
        let (_stats, _file) = self
            .agent
            .fs
            .create_file(rel, 0o644, 0, 0)
            .await
            .map_err(|e| format!("agentfs create {rel}: {e}"))?;
        self.agent
            .fs
            .pwrite(rel, 0, bytes)
            .await
            .map_err(|e| format!("agentfs write {rel}: {e}"))?;
        let abs = self.base.join(rel.trim_start_matches('/'));
        lock(&self.touched).push(abs.clone());
        if let Some(parent) = abs.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("agentfs host mkdir {}: {e}", parent.display()))?;
        }
        std::fs::write(&abs, bytes)
            .map_err(|e| format!("agentfs host write {}: {e}", abs.display()))?;
        lock(&self.touched).retain(|path| *path != abs);
        Ok(())
    }

    /// Record one mutation in the session's history. Recording marks the
    /// session file as carrying audit, so it survives the handle's drop.
    async fn record(&self, op: &str, path: &str, before: Option<&str>, after: &str) {
        self.recorded.fetch_add(1, Ordering::Relaxed);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let now_ms = now_ms();
        let key = format!("{}{:013}-{seq:06}", history_prefix(&self.session), now_ms);
        let record = HistoryRecord {
            op: op.to_string(),
            path: path.to_string(),
            before: before.map(str::to_string),
            after: after.to_string(),
            ts: now_ms,
        };
        if let Err(e) = self.agent.kv.set(&key, &record).await {
            tracing::warn!("agentfs history record failed: {e}");
        }
    }

    /// The session's file history, oldest first.    ///
    /// # Errors
    ///
    /// Fails when the history keys cannot be listed or a record does
    /// not decode.
    pub async fn history(&self) -> Result<Vec<HistoryRecord>, String> {
        let prefix = history_prefix(&self.session);
        let mut keys = self
            .agent
            .kv
            .keys()
            .await
            .map_err(|e| format!("agentfs history list: {e}"))?
            .into_iter()
            .filter(|key| key.starts_with(&prefix))
            .collect::<Vec<_>>();
        keys.sort();
        let mut records = Vec::with_capacity(keys.len());
        for key in keys {
            let record: Option<HistoryRecord> = self
                .agent
                .kv
                .get(&key)
                .await
                .map_err(|e| format!("agentfs history read {key}: {e}"))?;
            if let Some(record) = record {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Safety net for the end of a turn: materialize any path whose
    /// write-through failed mid-turn. Successful writes remove
    /// themselves from the pending list as they land, so a converged
    /// turn syncs nothing. The overlay keeps its copies either way — it
    /// stays the audit record; the host converges to it.
    ///
    /// # Errors
    ///
    /// Fails on the first path that cannot be read back or written out.
    pub async fn sync_to_host(&self) -> Result<Vec<PathBuf>, String> {
        let touched = lock(&self.touched).clone();
        // One write per path; content is read live so rewrites converge
        // to their final bytes.
        let mut seen = std::collections::HashSet::new();
        let mut synced = Vec::new();
        for abs in touched {
            if !seen.insert(abs.clone()) {
                continue;
            }
            let rel = self.overlay_path(&abs)?;
            let bytes = self
                .agent
                .fs
                .read_file(&rel)
                .await
                .map_err(|e| format!("agentfs sync read {}: {e}", abs.display()))?
                .ok_or_else(|| format!("agentfs sync: {rel} vanished mid-turn"))?;
            if let Some(parent) = abs.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("agentfs sync mkdir {}: {e}", parent.display()))?;
            }
            std::fs::write(&abs, &bytes)
                .map_err(|e| format!("agentfs sync write {}: {e}", abs.display()))?;
            synced.push(abs);
        }
        Ok(synced)
    }

    /// Union directory listing: overlay entries over host entries (the
    /// overlay wins on name conflicts — it is the newer side).
    async fn list_union(&self, abs: &Path) -> Result<Vec<(String, bool)>, String> {
        let mut merged = std::collections::BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(abs) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                merged.insert(name, is_dir);
            }
        }
        let rel = self.overlay_path(abs)?;
        if let Ok(Some(stats)) = self
            .agent
            .fs
            .lstat(&rel)
            .await
            .map_err(|e| format!("agentfs stat {}: {e}", abs.display()))
            && stats.is_directory()
            && let Ok(Some(entries)) = self
                .agent
                .fs
                .readdir_plus(stats.ino)
                .await
                .map_err(|e| format!("agentfs list {}: {e}", abs.display()))
        {
            for entry in entries {
                merged.insert(entry.name, entry.stats.is_directory());
            }
        }
        Ok(merged.into_iter().collect())
    }

    /// Walk the overlay tree under `rel`, yielding host-absolute paths.
    async fn walk_overlay(
        &self,
        rel: &str,
        abs: &Path,
        out: &mut Vec<PathBuf>,
    ) -> Result<(), String> {
        let stats = self
            .agent
            .fs
            .lstat(rel)
            .await
            .map_err(|e| format!("agentfs stat {rel}: {e}"))?;
        let Some(stats) = stats else { return Ok(()) };
        if !stats.is_directory() {
            return Ok(());
        }
        let entries = self
            .agent
            .fs
            .readdir_plus(stats.ino)
            .await
            .map_err(|e| format!("agentfs list {rel}: {e}"))?
            .unwrap_or_default();
        for entry in entries {
            let child_abs = abs.join(&entry.name);
            let child_rel = format!("{rel}/{}", entry.name);
            out.push(child_abs.clone());
            if entry.stats.is_directory() {
                self.walk_overlay_box(&child_rel, &child_abs, out).await?;
            }
        }
        Ok(())
    }

    /// Boxed recursion for [`AgentFsHandle::walk_overlay`] (else the
    /// future grows without bound).
    fn walk_overlay_box<'a>(
        &'a self,
        rel: &'a str,
        abs: &'a Path,
        out: &'a mut Vec<PathBuf>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(self.walk_overlay(rel, abs, out))
    }
}

/// Resolve a tool input path against the run's working directory.
/// Falls back to the process cwd when the host never registered a
/// [`Cwd`] component. Identical to the host fs plugin's resolution.
fn tool_path(ctx: &PluginContext, raw: &str) -> PathBuf {
    Cwd::from_ctx(ctx).resolve(raw)
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ReadInput {
    path: String,
    start_line: usize,
    /// Number of lines to read
    lines: usize,
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct WriteInput {
    path: String,
    content: String,
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ReplaceInput {
    path: String,
    old_string: String,
    new_string: String,
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct ListInput {
    /// Show tree up to this depth. Defaults to 2.
    depth: Option<usize>,
    path: String,
}

#[derive(JsonSchema, Deserialize, Serialize)]
struct GlobInput {
    pattern: String,
}

/// The agentfs filesystem plugin: [`AgentFsPlugin::new`] takes an open
/// [`AgentFsHandle`] (one per session). Tool for tool the surface
/// matches `agentsdk-plugin-fs`; only the backing store differs.
pub struct AgentFsPlugin {
    fs: AgentFsHandle,
}

impl AgentFsPlugin {
    /// Serve the fs tools over `fs`'s overlay + history.
    #[must_use]
    pub fn new(fs: AgentFsHandle) -> Self {
        Self { fs }
    }
}

impl std::fmt::Debug for AgentFsPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentFsPlugin")
            .field("session", &self.fs.session)
            .finish_non_exhaustive()
    }
}

#[derive(PluginTools, Serialize, Deserialize)]
enum FsTools {
    /// Read a file. UTF-8. Prefer reading partial content instead of whole file
    Read(ReadInput),
    /// Write a file. Overwrites if exists. Creates directories.
    Write(WriteInput),
    /// Surgical search and replace. Fails if `old_string` is not found or is ambiguous.
    #[tool(name = "Edit")]
    Replace(ReplaceInput),
    /// List directory entries.
    Ls(ListInput),
    /// List paths matching a pattern.
    Glob(GlobInput),
}

#[async_trait]
impl AgentPlugin for AgentFsPlugin {
    fn name(&self) -> &'static str {
        "fs-agentfs"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        FsTools::definitions()
    }

    async fn run_tool(
        &mut self,
        ctx: &mut PluginContext,
        call: &PluginToolCall,
    ) -> Result<Value, String> {
        match FsTools::from_call(call)? {
            FsTools::Read(input) => self.do_read(ctx, &input).await,
            FsTools::Write(input) => self.do_write(ctx, &input).await,
            FsTools::Replace(input) => self.do_replace(ctx, &input).await,
            FsTools::Ls(input) => self.do_list(ctx, &input).await,
            FsTools::Glob(input) => self.do_glob(ctx, &input).await,
        }
    }
}

impl AgentFsPlugin {
    async fn do_read(&self, ctx: &mut PluginContext, input: &ReadInput) -> Result<Value, String> {
        let abs = tool_path(ctx, &input.path);
        self.fs
            .policy
            .check_read_access(&abs)
            .map_err(|e| e.to_string())?;
        let content = self
            .fs
            .read_text(&abs)
            .await?
            .ok_or_else(|| format!("Failed to read {}: file not found", input.path))?;
        let all_lines: Vec<&str> = content.lines().collect();

        let start = input.start_line.max(1);
        let requested_lines = if input.lines == 0 { 10 } else { input.lines };
        let end = (start + requested_lines - 1).min(all_lines.len());

        if start > all_lines.len() {
            return Ok(json!("You have already read the full lines"));
        }

        let slice = all_lines
            .get(start.saturating_sub(1)..end)
            .ok_or("Invalid line range")?;
        let result_content = slice.join("\n");

        Ok(json!({
            "path": input.path,
            "content": result_content,
            "start_line": start,
            "end_line": end,
            "total_lines": all_lines.len()
        }))
    }

    async fn do_write(&self, ctx: &mut PluginContext, input: &WriteInput) -> Result<Value, String> {
        let abs = tool_path(ctx, &input.path);
        self.fs
            .policy
            .check_write_access(&abs)
            .map_err(|e| e.to_string())?;
        let before = self.fs.read_text(&abs).await?;
        let rel = self.fs.overlay_path(&abs)?;
        self.fs.write_bytes(&rel, input.content.as_bytes()).await?;
        self.fs
            .record("write", &input.path, before.as_deref(), &input.content)
            .await;
        Ok(json!({ "status": "success", "path": input.path, "bytes": input.content.len() }))
    }

    async fn do_replace(
        &self,
        ctx: &mut PluginContext,
        input: &ReplaceInput,
    ) -> Result<Value, String> {
        let abs = tool_path(ctx, &input.path);
        self.fs
            .policy
            .check_write_access(&abs)
            .map_err(|e| e.to_string())?;
        let content = self
            .fs
            .read_text(&abs)
            .await?
            .ok_or_else(|| format!("Failed to read: {} not found", input.path))?;
        let occurrences = content.matches(&input.old_string).count();
        if occurrences == 0 {
            return Err(format!("String not found in {}", input.path));
        }
        if occurrences > 1 {
            return Err(format!(
                "String found {occurrences} times in {}. Please provide more context to make it unique.",
                input.path
            ));
        }

        let new_content = content.replace(&input.old_string, &input.new_string);
        let rel = self.fs.overlay_path(&abs)?;
        self.fs.write_bytes(&rel, new_content.as_bytes()).await?;
        self.fs
            .record("edit", &input.path, Some(&content), &new_content)
            .await;

        Ok(json!({ "status": "success", "path": input.path }))
    }

    async fn do_list(&self, ctx: &mut PluginContext, input: &ListInput) -> Result<Value, String> {
        let max_depth = input.depth.unwrap_or(2);
        let abs = tool_path(ctx, &input.path);
        self.fs
            .policy
            .check_read_access(&abs)
            .map_err(|e| e.to_string())?;
        // Missing in both layers reads exactly like the host plugin's
        // missing directory; an existing-but-empty directory renders
        // the `(empty)` marker instead.
        let rel = self.fs.overlay_path(&abs)?;
        let overlay_exists = self
            .fs
            .agent
            .fs
            .lstat(&rel)
            .await
            .map_err(|e| format!("agentfs stat {}: {e}", abs.display()))?
            .is_some();
        if !overlay_exists && std::fs::read_dir(&abs).is_err() {
            return Err(format!(
                "Failed to list directory: {} not found",
                input.path
            ));
        }
        let tree = self.fs.collect_tree(&abs, max_depth).await?;
        Ok(json!(
            render_collected(&tree, &abs, "", true, 0, max_depth).trim_end()
        ))
    }

    async fn do_glob(&self, ctx: &mut PluginContext, input: &GlobInput) -> Result<Value, String> {
        let abs_pattern = tool_path(ctx, &input.pattern)
            .to_string_lossy()
            .into_owned();
        let compiled = glob::Pattern::new(&abs_pattern).map_err(|e| format!("Glob error: {e}"))?;
        let mut matches = std::collections::BTreeSet::new();
        // Host side, like the host plugin (policy-checked per match).
        for entry in glob::glob(&abs_pattern).map_err(|e| format!("Glob error: {e}"))? {
            let path = entry.map_err(|e| format!("Glob error: {e}"))?;
            self.fs
                .policy
                .check_read_access(&path)
                .map_err(|e| e.to_string())?;
            matches.insert(path.to_string_lossy().into_owned());
        }
        // Overlay side: walk tracked files and match the same pattern.
        let mut overlay_paths = Vec::new();
        self.fs
            .walk_overlay("/", &self.fs.base, &mut overlay_paths)
            .await?;
        for path in overlay_paths {
            if compiled.matches(&path.to_string_lossy())
                && self.fs.policy.check_read_access(&path).is_ok()
            {
                matches.insert(path.to_string_lossy().into_owned());
            }
        }
        Ok(json!({ "pattern": input.pattern, "matches": matches.into_iter().collect::<Vec<_>>() }))
    }
}

impl AgentFsHandle {
    /// Walk the union tree to `max_depth`, collecting each directory's
    /// merged listing for the synchronous renderer.
    async fn collect_tree(
        &self,
        root: &Path,
        max_depth: usize,
    ) -> Result<std::collections::BTreeMap<PathBuf, Vec<(String, bool)>>, String> {
        let mut tree = std::collections::BTreeMap::new();
        let mut stack = vec![(root.to_path_buf(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            if depth > max_depth {
                continue;
            }
            let entries = self.list_union(&dir).await?;
            for (name, is_dir) in &entries {
                if *is_dir && depth < max_depth {
                    stack.push((dir.join(name), depth + 1));
                }
            }
            tree.insert(dir, entries);
        }
        Ok(tree)
    }
}

/// Render one collected level exactly like the host fs plugin's tree:
/// brace-grouped files by extension, directories with subtrees.
fn render_collected(
    tree: &std::collections::BTreeMap<PathBuf, Vec<(String, bool)>>,
    dir: &Path,
    prefix: &str,
    is_root: bool,
    depth: usize,
    max_depth: usize,
) -> String {
    use std::fmt::Write as _;

    let entries = tree.get(dir).cloned().unwrap_or_default();
    if is_root && entries.is_empty() {
        return "(empty)\n".to_string();
    }

    let mut out = String::new();

    if is_root {
        let _ = writeln!(out, "{}", dir.to_string_lossy());
    }

    if depth >= max_depth {
        return out;
    }

    let mut dirs: Vec<&str> = Vec::new();
    let mut files: Vec<&str> = Vec::new();
    for (name, is_dir) in &entries {
        if *is_dir {
            dirs.push(name);
        } else {
            files.push(name);
        }
    }
    dirs.sort_unstable();
    files.sort_unstable();

    let mut by_ext: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
    for name in &files {
        let dot = name.rfind('.').filter(|&i| i > 0);
        let ext = dot.map_or("", |i| name.split_at(i).1);
        by_ext.entry(ext).or_default().push(name);
    }

    for ext_files in by_ext.values() {
        let Some(first) = ext_files.first() else {
            continue;
        };
        let entry = if ext_files.len() == 1 {
            (*first).to_string()
        } else {
            let stems: Vec<&str> = ext_files
                .iter()
                .map(|n| {
                    let dot = n.rfind('.').filter(|&i| i > 0);
                    dot.map_or(*n, |i| n.split_at(i).0)
                })
                .collect();
            let dot = first.rfind('.').filter(|&i| i > 0);
            match dot {
                Some(i) => {
                    let (_, ext) = first.split_at(i);
                    format!("{{{}}}{ext}", stems.join(","))
                }
                None => format!("{{{}}}", stems.join(",")),
            }
        };
        let _ = writeln!(out, "{prefix}{entry}");
    }

    for name in dirs {
        let child_path = dir.join(name);
        let child_prefix = format!("{prefix}  ");
        let _ = writeln!(out, "{prefix}{name}/");
        out.push_str(&render_collected(
            tree,
            &child_path,
            &child_prefix,
            false,
            depth + 1,
            max_depth,
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use agentsdk::core::plugin::PluginToolCall;

    /// A provider-`none` policy: the checks pass everything through and
    /// the tests exercise the overlay, not the sandbox lists.
    fn open_policy(base: &Path) -> PlatformSandbox {
        PlatformSandbox::new(p1e_sandbox::SandboxConfig::default(), base.to_path_buf())
    }

    async fn test_handle(base: &Path, session: &str) -> (tempfile::TempDir, AgentFsHandle) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(format!("{session}.db"));
        let fs = AgentFsHandle::open(&db, base, session, open_policy(base))
            .await
            .unwrap();
        (dir, fs)
    }

    /// A tool context rooted at `base` (no process-cwd mutation).
    fn test_ctx(base: &Path) -> PluginContext {
        let mut world = hecs::World::new();
        let entity = world.spawn(());
        world.insert_one(entity, Cwd(base.to_path_buf())).unwrap();
        PluginContext::new(world, entity)
    }

    fn call(id: &str, name: &str, arguments: Value) -> PluginToolCall {
        PluginToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
        }
    }

    #[tokio::test]
    async fn write_lands_on_the_host_immediately_and_journals() {
        let base = tempfile::tempdir().unwrap();
        let (_db, fs) = test_handle(base.path(), "s-1").await;
        let mut plugin = AgentFsPlugin::new(fs.clone());
        let mut ctx = test_ctx(base.path());

        // A host file the agent has not touched.
        std::fs::write(base.path().join("real.txt"), "host content").unwrap();

        let out = plugin
            .run_tool(
                &mut ctx,
                &call(
                    "1",
                    "Write",
                    json!({"path": "new.txt", "content": "tracked"}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "success");

        // The host has the bytes NOW — without any end-of-turn sync. The
        // shell (grep, cargo, git) shares one tree with the fs tools.
        assert_eq!(
            std::fs::read_to_string(base.path().join("new.txt")).unwrap(),
            "tracked"
        );
        // Reads see it too.
        let out = plugin
            .run_tool(
                &mut ctx,
                &call(
                    "2",
                    "Read",
                    json!({"path": "new.txt", "start_line": 1, "lines": 10}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(out["content"], "tracked");

        // Untouched host files still read through.
        let out = plugin
            .run_tool(
                &mut ctx,
                &call(
                    "3",
                    "Read",
                    json!({"path": "real.txt", "start_line": 1, "lines": 10}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(out["content"], "host content");

        // History recorded the mutation with its before (missing) side.
        let history = fs.history().await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].op, "write");
        assert_eq!(history[0].path, "new.txt");
        assert_eq!(history[0].before, None);
        assert_eq!(history[0].after, "tracked");
    }

    #[tokio::test]
    async fn edit_lands_on_the_host_and_journals_both_sides() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("code.txt"), "aaa\nbbb\nccc\n").unwrap();
        let (_db, fs) = test_handle(base.path(), "s-2").await;
        let mut plugin = AgentFsPlugin::new(fs.clone());
        let mut ctx = test_ctx(base.path());

        // Ambiguous edits are refused exactly like the host plugin.
        let err = plugin
            .run_tool(
                &mut ctx,
                &call("1", "Edit", json!({"path": "code.txt", "old_string": "aaa\nbbb\nccc\nx", "new_string": "?" })),
            )
            .await
            .expect_err("missing string must fail");
        assert!(err.contains("String not found"), "{err}");

        let out = plugin
            .run_tool(
                &mut ctx,
                &call(
                    "2",
                    "Edit",
                    json!({"path": "code.txt", "old_string": "bbb", "new_string": "BBB"}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "success");

        // The host file carries the edit; reads agree.
        assert_eq!(
            std::fs::read_to_string(base.path().join("code.txt")).unwrap(),
            "aaa\nBBB\nccc\n"
        );
        let out = plugin
            .run_tool(
                &mut ctx,
                &call(
                    "3",
                    "Read",
                    json!({"path": "code.txt", "start_line": 1, "lines": 10}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(out["content"], "aaa\nBBB\nccc");

        let history = fs.history().await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].op, "edit");
        assert_eq!(history[0].before.as_deref(), Some("aaa\nbbb\nccc\n"));
        assert_eq!(history[0].after, "aaa\nBBB\nccc\n");
    }

    /// The 2026-09-26 session failure: after a Write, something outside
    /// the fs tools (shell, git apply, the user) changes the host file.
    /// The next Edit must operate on the fresh host bytes — never on a
    /// stale overlay copy that makes `old_string` "not found".
    #[tokio::test]
    async fn edit_sees_host_changes_made_outside_the_overlay() {
        let base = tempfile::tempdir().unwrap();
        let (_db, fs) = test_handle(base.path(), "s-fresh").await;
        let mut plugin = AgentFsPlugin::new(fs.clone());
        let mut ctx = test_ctx(base.path());

        plugin
            .run_tool(
                &mut ctx,
                &call("1", "Write", json!({"path": "f.rs", "content": "v1\n"})),
            )
            .await
            .unwrap();

        // External mutation of the host file (as a shell command would).
        std::fs::write(base.path().join("f.rs"), "v1\npatched by shell\n").unwrap();

        // An edit against the externally-added content must find it.
        let out = plugin
            .run_tool(
                &mut ctx,
                &call(
                    "2",
                    "Edit",
                    json!({"path": "f.rs", "old_string": "patched by shell", "new_string": "patched by edit"}),
                ),
            )
            .await
            .unwrap();
        assert_eq!(out["status"], "success");
        assert_eq!(
            std::fs::read_to_string(base.path().join("f.rs")).unwrap(),
            "v1\npatched by edit\n"
        );
        // The journal's before-side is the true pre-edit bytes.
        let history = fs.history().await.unwrap();
        assert_eq!(
            history.last().unwrap().before.as_deref(),
            Some("v1\npatched by shell\n")
        );
    }

    /// A cancelled turn drops its handle without any end-of-turn sync;
    /// write-through means the host keeps what the turn wrote anyway.
    #[tokio::test]
    async fn dropped_handle_keeps_host_writes_without_sync() {
        let base = tempfile::tempdir().unwrap();
        let (_db, fs) = test_handle(base.path(), "s-cancel").await;
        let mut plugin = AgentFsPlugin::new(fs.clone());
        let mut ctx = test_ctx(base.path());
        plugin
            .run_tool(
                &mut ctx,
                &call("1", "Write", json!({"path": "keep.txt", "content": "kept"})),
            )
            .await
            .unwrap();
        drop(plugin);
        drop(fs);
        assert_eq!(
            std::fs::read_to_string(base.path().join("keep.txt")).unwrap(),
            "kept"
        );
    }

    #[tokio::test]
    async fn sessions_share_host_truth_but_keep_their_own_history() {
        let base = tempfile::tempdir().unwrap();
        let (_a, fa) = test_handle(base.path(), "s-a").await;
        let (_b, fb) = test_handle(base.path(), "s-b").await;

        fa.write_bytes("/shared.txt", b"from-a").await.unwrap();
        fa.record("write", "shared.txt", None, "from-a").await;
        // Writes land on the shared host, so the other session reads the
        // same bytes — one tree, not two.
        assert_eq!(
            fb.read_text(&base.path().join("shared.txt")).await.unwrap(),
            Some("from-a".to_string())
        );
        assert_eq!(
            fa.read_text(&base.path().join("shared.txt"))
                .await
                .unwrap()
                .as_deref(),
            Some("from-a")
        );
        // …but each session's history stays its own.
        assert_eq!(fa.history().await.unwrap().len(), 1);
        assert!(fb.history().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn history_survives_a_reopen() {
        let base = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.db");
        let fa = AgentFsHandle::open(&db, base.path(), "s-r", open_policy(base.path()))
            .await
            .unwrap();
        fa.write_bytes("/a.txt", b"one").await.unwrap();
        fa.record("write", "a.txt", None, "one").await;
        drop(fa);

        // A fresh handle over the same file continues the sequence —
        // per-turn handles never collide on keys.
        let fb = AgentFsHandle::open(&db, base.path(), "s-r", open_policy(base.path()))
            .await
            .unwrap();
        fb.write_bytes("/b.txt", b"two").await.unwrap();
        fb.record("write", "b.txt", None, "two").await;
        let history = fb.history().await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].after, "one");
        assert_eq!(history[1].after, "two");
    }

    #[tokio::test]
    async fn ls_merges_overlay_and_host() {
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("host.txt"), "h").unwrap();
        let (_db, fs) = test_handle(base.path(), "s-ls").await;
        fs.write_bytes("/over.txt", b"o").await.unwrap();
        let mut plugin = AgentFsPlugin::new(fs);
        let mut ctx = test_ctx(base.path());

        let out = plugin
            .run_tool(&mut ctx, &call("1", "Ls", json!({"path": ".", "depth": 1})))
            .await
            .unwrap();
        let tree = out.as_str().unwrap();
        // Same brace-grouping as the host plugin: same-extension files
        // merge, so both sides' files appear in one entry.
        assert!(tree.contains("{host,over}.txt"), "{tree}");
    }

    #[tokio::test]
    async fn missing_paths_read_like_the_host_plugin() {
        let base = tempfile::tempdir().unwrap();
        let (_db, fs) = test_handle(base.path(), "s-m").await;
        let mut plugin = AgentFsPlugin::new(fs);
        let mut ctx = test_ctx(base.path());

        let err = plugin
            .run_tool(
                &mut ctx,
                &call(
                    "1",
                    "Read",
                    json!({"path": "nope.txt", "start_line": 1, "lines": 5}),
                ),
            )
            .await
            .expect_err("missing files must fail");
        assert!(err.contains("Failed to read"), "{err}");

        let err = plugin
            .run_tool(
                &mut ctx,
                &call("2", "Ls", json!({"path": "nope", "depth": 1})),
            )
            .await
            .expect_err("missing dirs must fail");
        assert!(err.contains("Failed to list directory"), "{err}");
    }

    #[tokio::test]
    async fn paths_outside_the_base_are_refused() {
        let base = tempfile::tempdir().unwrap();
        let (_db, fs) = test_handle(base.path(), "s-c").await;
        let mut plugin = AgentFsPlugin::new(fs);
        let mut ctx = test_ctx(base.path());

        let err = plugin
            .run_tool(
                &mut ctx,
                &call(
                    "1",
                    "Write",
                    json!({"path": "../escape.txt", "content": "x"}),
                ),
            )
            .await
            .expect_err("escaping the base must fail");
        assert!(
            err.contains("outside the agentfs base") || err.contains("not allowed"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn successful_write_throughs_leave_sync_nothing_to_do() {
        let base = tempfile::tempdir().unwrap();
        let (_db, fs) = test_handle(base.path(), "s-sync").await;
        let mut plugin = AgentFsPlugin::new(fs.clone());
        let mut ctx = test_ctx(base.path());

        // Nested path: parents are created on write-through. Rewrites
        // converge — the host always carries the latest bytes.
        for (id, content) in [("1", "v1"), ("2", "v2")] {
            plugin
                .run_tool(
                    &mut ctx,
                    &call(
                        id,
                        "Write",
                        json!({"path": "sub/dir/f.txt", "content": content}),
                    ),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(base.path().join("sub/dir/f.txt")).unwrap(),
            "v2",
            "the host is current before any sync"
        );

        // Every write already converged, so the end-of-turn safety net
        // has nothing pending — and syncing is a no-op.
        let synced = fs.sync_to_host().await.unwrap();
        assert!(synced.is_empty(), "converged writes leave no sync work");
        assert_eq!(
            std::fs::read_to_string(base.path().join("sub/dir/f.txt")).unwrap(),
            "v2"
        );
        // History still holds both mutations — the audit is untouched.
        assert_eq!(fs.history().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sync_with_nothing_touched_syncs_nothing() {
        let base = tempfile::tempdir().unwrap();
        let (_db, fs) = test_handle(base.path(), "s-empty").await;
        assert!(fs.sync_to_host().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn untouched_fresh_files_vanish_on_drop() {
        let base = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("drop.db");
        {
            let _fs = AgentFsHandle::open(&db, base.path(), "s-drop", open_policy(base.path()))
                .await
                .unwrap();
        }
        assert!(!db.exists(), "a turn that wrote nothing leaves no file");
    }

    #[tokio::test]
    async fn files_with_history_survive_later_read_only_handles() {
        let base = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("keep.db");
        {
            let fs = AgentFsHandle::open(&db, base.path(), "s-keep", open_policy(base.path()))
                .await
                .unwrap();
            fs.write_bytes("/k.txt", b"v").await.unwrap();
            fs.record("write", "k.txt", None, "v").await;
        }
        assert!(db.exists());
        // A later read-only turn over the same file must not wipe the
        // earlier turn's audit when it drops.
        {
            let _fs = AgentFsHandle::open(&db, base.path(), "s-keep", open_policy(base.path()))
                .await
                .unwrap();
        }
        assert!(db.exists(), "history must survive read-only handles");
    }
}
