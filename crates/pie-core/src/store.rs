//! pie-core's persistence seams: LLM usage bookkeeping and MCP OAuth
//! grants.
//!
//! pie-core holds no SQL driver — the durable tables live where the SQL
//! lives (pie-acp's `store`, over the same `~/.pie/pie.db` the daemon
//! always used), and everything here flows through these two traits:
//! [`UsageStore`] (record + aggregate per-run token spend) and
//! [`TokenStore`] (load/save/clear one JSON blob per MCP server).
//! [`TokenCredentialStore`] adapts a [`TokenStore`] to rmcp's
//! `CredentialStore`, so the OAuth flow never sees SQL either.
//! [`MemoryStore`] is the in-memory implementation for tests.

use crate::usage::{ModelUsage, RunUsage, to_f64};
use rmcp::transport::auth::{AuthError, CredentialStore, StoredCredentials};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

fn lock<T>(guard: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    guard.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One run's billable footprint: the token counts plus the display id of
/// the in-memory session that spent them (grouping only — sessions are
/// never persisted) and the USD cost at the configured pricing (`None`
/// when the model has no `[pricing.*]` entry).
#[derive(Debug, Clone)]
pub struct UsageEntry {
    pub session_id: String,
    pub model: String,
    pub agent: Option<String>,
    pub usage: RunUsage,
    pub cost_usd: Option<f64>,
}

/// The usage bookkeeping table behind `pie usage`: one row per run,
/// aggregated per model over a window (`since_ms = 0` means all time).
#[async_trait::async_trait]
pub trait UsageStore: Send + Sync {
    /// Persist one run's [`UsageEntry`].
    async fn record_usage(&self, entry: UsageEntry) -> anyhow::Result<()>;
    /// Aggregate rows per model within the window, ordered by cost
    /// (unpriced last), then by token volume.
    async fn usage_by_model(&self, since_ms: i64) -> anyhow::Result<Vec<ModelUsage>>;
}

/// The MCP OAuth grant table: one JSON blob (rmcp's `StoredCredentials`)
/// per configured server, written by `pie mcp login` and read on every
/// run that connects the server.
#[async_trait::async_trait]
pub trait TokenStore: Send + Sync {
    /// The stored credentials JSON for `server`, if `pie mcp login` ever
    /// wrote one.
    async fn load_token(&self, server: &str) -> anyhow::Result<Option<String>>;
    /// Upsert the credentials JSON for `server`.
    async fn save_token(&self, server: &str, json: String) -> anyhow::Result<()>;
    /// Forget `server`'s credentials (`pie mcp logout`).
    async fn clear_token(&self, server: &str) -> anyhow::Result<()>;
}

/// rmcp's [`CredentialStore`] over any [`TokenStore`] — the OAuth flow's
/// only view of persistence. The JSON translation lives here, so the SQL
/// implementation deals in plain strings.
pub struct TokenCredentialStore {
    tokens: Arc<dyn TokenStore>,
    server: String,
}

impl TokenCredentialStore {
    /// Serve `server`'s grants from `tokens`.
    pub fn new(tokens: Arc<dyn TokenStore>, server: &str) -> Self {
        Self {
            tokens,
            server: server.to_string(),
        }
    }
}

impl std::fmt::Debug for TokenCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenCredentialStore")
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl CredentialStore for TokenCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let row = self
            .tokens
            .load_token(&self.server)
            .await
            .map_err(store_err)?;
        row.map(|json| serde_json::from_str(&json).map_err(store_err))
            .transpose()
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let json = serde_json::to_string(&credentials).map_err(store_err)?;
        self.tokens
            .save_token(&self.server, json)
            .await
            .map_err(store_err)?;
        Ok(())
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.tokens
            .clear_token(&self.server)
            .await
            .map_err(store_err)?;
        Ok(())
    }
}

fn store_err(e: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(e.to_string())
}

/// The in-memory [`UsageStore`] + [`TokenStore`] for tests: every engine
/// and assembly takes the traits, so no test needs SQLite.
#[derive(Debug, Default)]
pub struct MemoryStore {
    usage: Mutex<Vec<UsageEntry>>,
    tokens: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    /// An empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl UsageStore for MemoryStore {
    async fn record_usage(&self, entry: UsageEntry) -> anyhow::Result<()> {
        lock(&self.usage).push(entry);
        Ok(())
    }

    async fn usage_by_model(&self, since_ms: i64) -> anyhow::Result<Vec<ModelUsage>> {
        let entries = lock(&self.usage).clone();
        // The in-memory store stamps nothing (no clock column) — it keeps
        // insertion order, so a nonzero window has nothing to filter on
        // and reads as empty; the SQL implementation filters on `ts`.
        // Tests aggregate with `since_ms = 0` (all time).
        if since_ms != 0 {
            return Ok(Vec::new());
        }
        let mut by_model: HashMap<String, crate::usage::UsageReport> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        for entry in &entries {
            if !by_model.contains_key(&entry.model) {
                order.push(entry.model.clone());
            }
            let report = by_model
                .entry(entry.model.clone())
                .or_insert_with(|| entry.usage.report(entry.cost_usd));
            // Accumulate raw counts; cost follows the priced rows.
            report.requests += entry.usage.requests;
            report.prompt_tokens += entry.usage.prompt_tokens;
            report.completion_tokens += entry.usage.completion_tokens;
            report.total_tokens += entry.usage.total_tokens;
            report.cached_tokens += entry.usage.cached_tokens;
            report.reasoning_tokens += entry.usage.reasoning_tokens;
            report.cost_usd = match (report.cost_usd, entry.cost_usd) {
                (acc, None) => acc,
                (None, Some(c)) => Some(c),
                (Some(acc), Some(c)) => Some(acc + c),
            };
        }
        let mut rows: Vec<ModelUsage> = Vec::new();
        for model in order {
            let Some(mut usage) = by_model.remove(&model) else {
                continue;
            };
            usage.cache_rate = (usage.prompt_tokens > 0)
                .then(|| to_f64(usage.cached_tokens) / to_f64(usage.prompt_tokens));
            rows.push(ModelUsage { model, usage });
        }
        rows.sort_by(|a, b| {
            // Priced rows first (unpriced last), then cost descending,
            // then token volume descending — the `pie usage` order.
            a.usage
                .cost_usd
                .is_none()
                .cmp(&b.usage.cost_usd.is_none())
                .then_with(|| {
                    b.usage
                        .cost_usd
                        .partial_cmp(&a.usage.cost_usd)
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| b.usage.total_tokens.cmp(&a.usage.total_tokens))
        });
        Ok(rows)
    }
}

#[async_trait::async_trait]
impl TokenStore for MemoryStore {
    async fn load_token(&self, server: &str) -> anyhow::Result<Option<String>> {
        Ok(lock(&self.tokens).get(server).cloned())
    }

    async fn save_token(&self, server: &str, json: String) -> anyhow::Result<()> {
        lock(&self.tokens).insert(server.to_string(), json);
        Ok(())
    }

    async fn clear_token(&self, server: &str) -> anyhow::Result<()> {
        lock(&self.tokens).remove(server);
        Ok(())
    }
}
