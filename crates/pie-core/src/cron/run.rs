use crate::cron::condition::{self, ConditionContext};
use crate::cron::executor::prompt_exec;
use crate::cron::models::CronRun;
use crate::cron::schedule::{Schedule, load_all_schedules};
use crate::db::DbPool;
use crate::registry::Registry;
use crate::session::{HistoryContent, HistoryEntry, Session};
use chrono::{DateTime, Utc};
use croner::Cron;
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

fn logs_dir() -> PathBuf {
    crate::config::pie_home().join("logs")
}

fn format_history(schedule_id: &str, run_id: &str, entries: &[HistoryEntry]) -> String {
    let mut buf = String::new();
    writeln!(buf, "=== {schedule_id} ({run_id}) ===").ok();
    for entry in entries {
        match entry.to_history_content() {
            Ok(HistoryContent::User(c)) => writeln!(buf, "\n[USER] {c}"),
            Ok(HistoryContent::Assistant(c)) => writeln!(buf, "\n[ASSISTANT] {c}"),
            Ok(HistoryContent::System(c)) => writeln!(buf, "\n[SYSTEM] {c}"),
            Ok(HistoryContent::Tool(tc)) => {
                let s = serde_json::to_string(&tc).unwrap_or_default();
                writeln!(buf, "\n[TOOL] {s}")
            }
            Err(_) => Ok(()),
        }
        .ok();
    }
    buf
}

fn save_log(schedule_id: &str, run_id: &str, content: &str) {
    let dir = logs_dir().join(schedule_id);
    if let Err(e) = fs::create_dir_all(&dir) {
        tracing::error!("failed to create log dir {dir:?}: {e}");
        return;
    }
    let path = dir.join(format!("{run_id}.log"));
    if let Err(e) = fs::write(&path, content) {
        tracing::error!("failed to write log {path:?}: {e}");
    }
}

/// Why a schedule is or is not firing this tick.
enum Due {
    Yes,
    /// A legitimate "not now" — the normal case, logged at debug.
    No(String),
    /// The schedule is malformed. Never fires, and is worth an error.
    Invalid(String),
}

/// Decide whether a schedule should fire.
///
/// Two independent gates: `cron` answers "is it time yet", `when` answers "is
/// the world in the state that makes this worth doing". A schedule may set
/// either or both, and both must pass. A `when` that cannot be evaluated is
/// never treated as satisfied — a condition that silently fired every tick
/// would be the failure that went unnoticed longest.
fn is_due(sched: &Schedule, last_started: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Due {
    if !sched.has_trigger() {
        return Due::Invalid("has neither 'cron' nor 'when' and can never fire".to_string());
    }

    // First run fires immediately regardless of cron timing.
    if let Some(cron_expr) = &sched.cron
        && let Some(reference) = last_started
    {
        let cron = match Cron::from_str(cron_expr) {
            Ok(cron) => cron,
            Err(e) => return Due::Invalid(format!("bad cron '{cron_expr}': {e}")),
        };
        let next = match cron.find_next_occurrence(&reference, false) {
            Ok(dt) => dt,
            Err(e) => return Due::Invalid(format!("bad cron '{cron_expr}': {e}")),
        };
        if now < next {
            return Due::No(format!("not until {}", next.format("%Y-%m-%d %H:%M:%S")));
        }
    }

    if let Some(expr) = &sched.when {
        let ctx = ConditionContext {
            now,
            last_run: last_started,
        };
        match condition::evaluate(expr, &ctx) {
            Ok(true) => {}
            Ok(false) => return Due::No(format!("when is false: {expr}")),
            Err(e) => return Due::Invalid(format!("when '{expr}' — {e}")),
        }
    }

    Due::Yes
}

pub async fn run_due_jobs(pool: Arc<DbPool>, registry: Arc<Registry>) -> anyhow::Result<()> {
    // Clean up stale runs before checking for due jobs
    CronRun::cleanup_stale(&pool).await?;

    let schedules = load_all_schedules();
    for sched in &schedules {
        if !sched.enabled {
            tracing::debug!("schedule '{}' is disabled, skipping", sched.id);
            continue;
        }

        let now = Utc::now();
        let last_run = CronRun::last_run_for_schedule(&pool, &sched.id).await?;
        let last_started = last_run
            .as_ref()
            .and_then(|run| DateTime::from_timestamp_millis(run.started_at));

        match is_due(sched, last_started, now) {
            Due::Yes => {}
            Due::No(why) => {
                tracing::debug!("schedule '{}' not due: {why}", sched.id);
                continue;
            }
            Due::Invalid(why) => {
                // One broken schedule must not abort the whole pass.
                tracing::error!("schedule '{}': {why}", sched.id);
                continue;
            }
        }

        if CronRun::is_running_for_schedule(&pool, &sched.id).await? {
            tracing::warn!("schedule '{}' already running, skipping", sched.id);
            continue;
        }

        tracing::info!("running schedule: {}", sched.id);

        let mut session = Session::create_with_parent(pool.clone(), Some(&sched.id)).await?;
        let cron_run = CronRun::start(&pool, &sched.id, &session.id.to_string()).await?;

        let cwd = sched
            .source_path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.to_str())
            .unwrap_or(".");

        let sandbox = {
            let mut cfg = crate::config::build_sandbox(&crate::config::load_config()?)
                .as_ref()
                .clone();
            if let Some(sb) = &sched.sandbox {
                cfg.merge(sb);
            }
            Arc::new(cfg)
        };

        let grants: HashSet<_> = sched.grants.iter().cloned().collect();

        let exit_code = prompt_exec(
            &mut session,
            &sched.prompt,
            cwd,
            registry.clone(),
            sandbox,
            grants,
        )
        .await;

        // Rebuild cache from DB — prompt_exec clones the session internally
        let _ = session.rebuild_cache().await;

        let notes = session
            .history_entries()
            .iter()
            .rev()
            .find_map(|e| match e.to_history_content() {
                Ok(HistoryContent::Assistant(c)) => Some(c),
                _ => None,
            })
            .unwrap_or_default();

        cron_run.finish(&pool, exit_code, &notes).await?;

        // Save log file and print to stdout
        let log = format_history(&sched.id, &cron_run.id, session.history_entries());
        save_log(&sched.id, &cron_run.id, &log);
        println!("{log}");

        let status = if exit_code == 0 { "ok" } else { "failed" };
        tracing::info!("schedule '{}': {status} (exit: {exit_code})", sched.id);
    }

    Ok(())
}
