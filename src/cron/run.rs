use crate::cron::executor::{prompt_exec, shell_exec};
use crate::cron::models::{CronJob, CronRun, JobType};
use crate::db::DbPool;
use crate::registry::Registry;
use crate::session::Session;
use std::sync::Arc;

pub async fn run_due_jobs(pool: Arc<DbPool>, registry: Arc<Registry>) -> anyhow::Result<()> {
    let jobs = CronJob::find_due(&pool).await?;
    if jobs.is_empty() {
        return Ok(());
    }

    for job in &jobs {
        tracing::info!("running cron job: {} ({})", job.name, job.id);

        if CronRun::find_running_for_cron(&pool, &job.id).await? {
            tracing::warn!("cron job {} already running, skipping", job.name);
            continue;
        }

        let mut session = Session::create_with_parent(pool.clone(), Some(&job.id)).await?;
        let cron_run = CronRun::start(&pool, &job.id, &session.id.to_string()).await?;

        let exit_code = match job.job_type {
            JobType::Shell => shell_exec(&mut session, &job.payload, &job.cwd).await,
            JobType::Prompt => {
                prompt_exec(
                    &mut session,
                    &job.payload,
                    &job.cwd,
                    registry.clone(),
                    pool.clone(),
                )
                .await
            }
        };

        cron_run.finish(&pool, exit_code).await?;
        job.update_next_run(&pool).await?;

        let status = if exit_code == 0 { "ok" } else { "failed" };
        tracing::info!("cron job {}: {status} (exit: {exit_code})", job.name);
    }

    Ok(())
}
