-- Remove DB-backed cron_jobs; schedules are now file-based.
-- Keep cron_runs for history, but drop FK constraint to cron_jobs.

DROP TABLE IF EXISTS cron_jobs;

ALTER TABLE cron_runs RENAME TO cron_runs_old;

CREATE TABLE cron_runs (
    id             TEXT PRIMARY KEY,
    cron_id        TEXT NOT NULL,
    session_id     TEXT NOT NULL REFERENCES sessions(id),
    status         TEXT NOT NULL DEFAULT 'pending'
                   CHECK(status IN ('pending', 'running', 'completed', 'failed')),
    started_at     INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000),
    finished_at    INTEGER,
    exit_code      INTEGER
);

INSERT INTO cron_runs (id, cron_id, session_id, status, started_at, finished_at, exit_code)
SELECT id, cron_id, session_id, status, started_at, finished_at, exit_code FROM cron_runs_old;

DROP TABLE cron_runs_old;

CREATE INDEX idx_cron_runs_cron_id ON cron_runs(cron_id);
