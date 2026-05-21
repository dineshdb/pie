use crate::cron::run::run_due_jobs;
use crate::db::DbPool;
use crate::registry::Registry;
use std::sync::Arc;
use std::time::Duration;

pub async fn run_daemon(
    pool: Arc<DbPool>,
    registry: Arc<Registry>,
    interval_secs: u64,
) -> anyhow::Result<()> {
    let interval = Duration::from_secs(interval_secs);

    tracing::info!(
        "daemon started (check interval: {interval_secs}s, pid: {})",
        std::process::id(),
    );

    if let Err(e) = run_due_jobs(pool.clone(), registry.clone()).await {
        tracing::error!("daemon: initial job run error: {e}");
    }

    loop {
        let step = Duration::from_secs(1);
        let total = interval;
        let mut elapsed = Duration::ZERO;
        let mut should_stop = false;

        while elapsed < total {
            tokio::select! {
                () = tokio::time::sleep(step) => {
                    elapsed += step;
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("daemon: received Ctrl+C, exiting");
                    should_stop = true;
                    break;
                }
            }
        }

        if should_stop {
            break;
        }

        tracing::debug!("daemon: checking for due schedules");
        if let Err(e) = run_due_jobs(pool.clone(), registry.clone()).await {
            tracing::error!("daemon: job execution error: {e}");
        }
    }

    Ok(())
}
