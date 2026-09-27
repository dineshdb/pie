//! The A2A wire client: everything pie-tui needs to talk to the gateway
//! daemon over HTTP, with no gateway library. One JSON-RPC endpoint
//! (`POST {base}/a2a` — streaming methods answer SSE `data:` lines,
//! everything else a JSON envelope), the agent card at
//! `GET /.well-known/agent-card.json`, and the daemon's documented wire
//! extensions (`x_warm`, `x_contextHistory`, `x_lastContextForCwd`,
//! `x_selection`) beside the spec methods. The daemon implements this
//! contract (a2acp's docs/A2A.md); this module is the client's side of
//! it — same process boundary as any other A2A client.
//!
//! The daemon rendezvous ([`ensure_daemon`]) speaks the pidfile
//! convention, not the gateway's code: a JSON `{pid, baseUrl}` file the
//! daemon publishes under the runtime dir (config dir as the fallback).
//! A healthy daemon is adopted; an absent one is spawned detached and
//! polled until its card answers.

use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

/// The gateway daemon's published rendezvous: process id and HTTP base
/// (`http://127.0.0.1:8631`) — the pidfile's exact shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub base_url: String,
}

/// A conversation's current selection, as the gateway holds it (the
/// read-back of the selection extension's request payload).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One transcript entry of a conversation's whole history: the role,
/// the flattened text, and the turn (`taskId`) it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextMessage {
    pub task_id: String,
    pub role: String,
    pub text: String,
    pub message_id: String,
}

/// The reply to one JSON-RPC call: a single envelope, or — for the
/// streaming methods — the SSE `data:` payloads in stream order, each
/// one serialized JSON.
#[derive(Debug)]
pub enum A2aReply {
    Envelope(Value),
    Stream(mpsc::Receiver<String>),
}

/// An A2A client onto one daemon: `base_url` is its HTTP root
/// (`http://127.0.0.1:8631`).
#[derive(Debug, Clone)]
pub struct A2aClient {
    base_url: String,
    http: reqwest::Client,
}

impl A2aClient {
    /// A client onto the daemon at `base_url`.
    ///
    /// # Errors
    ///
    /// Errors if the HTTP client cannot be built (TLS backend failure —
    /// practically never).
    pub fn new(base_url: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Self {
            base_url: base_url.into(),
            http: reqwest::Client::builder().build()?,
        })
    }

    /// The daemon's HTTP root.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn rpc_url(&self) -> String {
        format!("{}/a2a", self.base_url)
    }

    /// One JSON-RPC call. Streaming methods (`SendStreamingMessage`)
    /// return [`A2aReply::Stream`]; everything else
    /// [`A2aReply::Envelope`].
    ///
    /// # Errors
    ///
    /// Errors on transport failures (daemon unreachable, timeouts);
    /// application errors arrive *inside* the envelope (`error` field).
    pub async fn call(&self, body: Value) -> anyhow::Result<A2aReply> {
        let response = self
            .http
            .post(self.rpc_url())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if content_type.starts_with("text/event-stream") {
            use futures::StreamExt as _;
            let (tx, rx) = mpsc::channel(64);
            tokio::spawn(async move {
                let mut stream = response.bytes_stream();
                let mut parser = SseParser::default();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            for payload in parser.feed(&bytes) {
                                if tx.send(payload).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("a2a stream: {e}");
                            break;
                        }
                    }
                }
                for payload in parser.finish() {
                    let _ = tx.send(payload).await;
                }
            });
            return Ok(A2aReply::Stream(rx));
        }
        let envelope: Value = response.json().await?;
        Ok(A2aReply::Envelope(envelope))
    }

    /// The daemon's agent card — the agent directory.
    ///
    /// # Errors
    ///
    /// Errors on transport failures; an absent daemon surfaces here.
    pub async fn card(&self) -> anyhow::Result<Value> {
        Ok(self
            .http
            .get(format!("{}/.well-known/agent-card.json", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// `x_warm` over the wire: open the agent's session before any
    /// message; the returned context id is the conversation to send on.
    /// `None` when the daemon refused (unknown agent, dead agent) —
    /// the lazy first-message open remains the fallback.
    pub async fn warm(&self, agent: &str, cwd: &std::path::Path) -> Option<String> {
        let Ok(A2aReply::Envelope(envelope)) = self
            .call(json!({
                "jsonrpc": "2.0", "id": 1, "method": "x_warm",
                "params": {"agent": agent, "cwd": cwd.display().to_string()},
            }))
            .await
        else {
            return None;
        };
        envelope
            .pointer("/result/contextId")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// `x_contextHistory` over the wire: the conversation's whole
    /// transcript, oldest first. Empty when unknown or unpersisted.
    pub async fn context_history(&self, context_id: &str) -> Vec<ContextMessage> {
        let Ok(A2aReply::Envelope(envelope)) = self
            .call(json!({
                "jsonrpc": "2.0", "id": 1, "method": "x_contextHistory",
                "params": {"contextId": context_id},
            }))
            .await
        else {
            return Vec::new();
        };
        envelope
            .pointer("/result/messages")
            .cloned()
            .and_then(|messages| serde_json::from_value(messages).ok())
            .unwrap_or_default()
    }

    /// `x_lastContextForCwd` over the wire: the directory's most recent
    /// conversation (the `--resume` lookup), if any.
    pub async fn last_context_for_cwd(&self, cwd: &std::path::Path) -> Option<String> {
        let Ok(A2aReply::Envelope(envelope)) = self
            .call(json!({
                "jsonrpc": "2.0", "id": 1, "method": "x_lastContextForCwd",
                "params": {"cwd": cwd.display().to_string()},
            }))
            .await
        else {
            return None;
        };
        envelope
            .pointer("/result/contextId")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// `x_selection` over the wire: the conversation's confirmed
    /// mode/model (the selection extension's read-back).
    pub async fn selection(&self, context_id: &str) -> Option<ConversationSelection> {
        let Ok(A2aReply::Envelope(envelope)) = self
            .call(json!({
                "jsonrpc": "2.0", "id": 1, "method": "x_selection",
                "params": {"contextId": context_id},
            }))
            .await
        else {
            return None;
        };
        envelope
            .pointer("/result/selection")
            .cloned()
            .and_then(|selection| serde_json::from_value(selection).ok())
    }
}

/// Incremental `data:` line extractor for an SSE stream. The daemon
/// writes each event as one `data: <json>` line (JSON single-line);
/// blank lines, comments, and any other field names skip through.
#[derive(Default)]
struct SseParser {
    pending: String,
}

impl SseParser {
    /// Consume one chunk of the byte stream; every complete `data:`
    /// payload comes back in order.
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.push_str(&String::from_utf8_lossy(bytes));
        let mut payloads = Vec::new();
        while let Some(pos) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=pos).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(payload) = line.strip_prefix("data: ") {
                payloads.push(payload.trim().to_string());
            }
        }
        payloads
    }

    /// What a stream end left without a trailing newline — tolerate it
    /// the same as a terminated line.
    fn finish(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.pending);
        let line = rest.trim_end_matches(['\n', '\r']);
        line.strip_prefix("data: ")
            .map(|payload| vec![payload.trim().to_string()])
            .unwrap_or_default()
    }
}

// ── the daemon rendezvous ──────────────────────────────────────────

/// How long [`ensure_daemon`] waits for a spawned daemon's card.
const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// The gateway daemon's role name — the pidfile stem it publishes.
const DAEMON_ROLE: &str = "a2acp";

/// Find the running gateway daemon, or start one: a healthy daemon
/// (per its pidfile) is adopted, an absent or stale one is spawned via
/// `spawn` (the platform argv) detached and polled until its card
/// answers. Returns the daemon's HTTP base.
///
/// # Errors
///
/// Errors when the daemon cannot be spawned or does not become ready
/// within [`READY_TIMEOUT`].
pub async fn ensure_daemon(
    spawn: &(impl Fn() -> std::process::Command + Send + Sync),
) -> anyhow::Result<String> {
    if let Some(info) = running_daemon().await {
        return Ok(info.base_url);
    }
    // A stale pidfile would sit in the way of the fresh one the spawned
    // daemon writes; the pid check already proved it is dead.
    remove_pidfile();
    let mut cmd = spawn();
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach from the caller's terminal: the daemon must survive the
    // spawning client's exit — its own process group is what makes
    // that true.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = tokio::process::Command::from(cmd)
        .spawn()
        .map_err(|e| anyhow::anyhow!("starting the {DAEMON_ROLE} daemon: {e}"))?;
    // Detached: the daemon outlives this client (that is the point).

    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        // Readiness = the daemon published its pidfile AND answers its
        // card. The pidfile is authoritative for the base URL.
        if let Some(info) = read_pidfile()
            && !info.base_url.is_empty()
            && card_answers(&info.base_url).await
        {
            // Reap so the daemon does not linger as this client's
            // zombie; it keeps running in its own process group.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            return Ok(info.base_url);
        }
        // A spawn that died (port taken, bad config) must fail fast
        // with its exit status, not burn the readiness window.
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!("the {DAEMON_ROLE} daemon exited before becoming ready ({status})");
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("the {DAEMON_ROLE} daemon did not become ready within {READY_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The daemon the pidfile names — only when the pid is alive AND the
/// base URL answers a card request. A dead pid's file is removed; a
/// live-but-unresponsive daemon is reported absent, not killed.
async fn running_daemon() -> Option<DaemonInfo> {
    let info = read_pidfile()?;
    if !pid_alive(info.pid) {
        remove_pidfile();
        return None;
    }
    if !card_answers(&info.base_url).await {
        return None;
    }
    Some(info)
}

/// Every pidfile location, read order: runtime dir first (freshest),
/// config dir second (the shared fallback — a launchd daemon and a
/// shell client disagree on `TMPDIR`/`XDG_RUNTIME_DIR`).
fn pidfile_paths() -> Vec<PathBuf> {
    let mut paths = vec![runtime_dir().join(format!("{DAEMON_ROLE}.pid"))];
    if let Some(dir) = app_dir() {
        paths.push(dir.join(format!("{DAEMON_ROLE}.pid")));
    }
    paths
}

/// The daemon's runtime directory (pidfile home): `$XDG_RUNTIME_DIR`
/// when set (the session's tmpfs), else `$TMPDIR/a2acp/runtime` — the
/// standard macOS situation, where `$TMPDIR` is already per-user.
fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join(DAEMON_ROLE);
    }
    let base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join(DAEMON_ROLE).join("runtime")
}

/// The daemon's config directory (`A2A_ACP_HOME` wins), the fallback
/// pidfile home.
fn app_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("A2A_ACP_HOME") {
        return Some(PathBuf::from(home));
    }
    dirs::config_dir().map(|dir| dir.join(DAEMON_ROLE))
}

fn read_pidfile() -> Option<DaemonInfo> {
    for path in pidfile_paths() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            // Corrupt: treated as absent (the writer replaces it).
            return serde_json::from_str(&text).ok();
        }
    }
    None
}

fn remove_pidfile() {
    for path in pidfile_paths() {
        let _ = std::fs::remove_file(path);
    }
}

/// `kill(pid, 0)` semantics without libc: the /proc-free macOS-safe
/// probe is `kill -0`; a pid that is not running fails.
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn card_answers(base_url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    matches!(
        client
            .get(format!("{base_url}/.well-known/agent-card.json"))
            .send()
            .await,
        Ok(response) if response.status().is_success()
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// Chunked SSE feeds parse exactly like whole-body ones: payloads
    /// come out in order, split lines wait for their remainder.
    #[test]
    fn sse_payloads_feed_incrementally() {
        let mut parser = SseParser::default();
        assert!(parser.feed(b"data: {\"a\":").is_empty());
        assert_eq!(parser.feed(b"1}\n\n:data comment\n"), vec![r#"{"a":1}"#]);
        assert_eq!(
            parser.feed(b"data: {\"b\":2}\n"),
            vec![r#"{"b":2}"#],
            "the comment line skipped, the next payload complete"
        );
        assert_eq!(parser.feed(b"data: trailing"), Vec::<String>::new());
        assert_eq!(
            parser.finish(),
            vec!["trailing"],
            "an unterminated tail still lands"
        );
    }

    /// Non-data lines (event names, retries, blanks) never surface.
    #[test]
    fn sse_ignores_everything_but_data_lines() {
        let mut parser = SseParser::default();
        let payloads = parser.feed(b"event: x\ndata: 1\n\n: keepalive\ndata: 2\n");
        assert_eq!(payloads, vec!["1", "2"]);
    }
}
