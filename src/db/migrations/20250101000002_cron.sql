ALTER TABLE sessions ADD COLUMN parent_id TEXT;
CREATE INDEX idx_sessions_parent_id ON sessions(parent_id);

CREATE TABLE cron_jobs (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL,
    type           TEXT NOT NULL CHECK(type IN ('shell', 'prompt')),
    payload        TEXT NOT NULL,
    cron           TEXT NOT NULL,
    cwd            TEXT NOT NULL DEFAULT '',
    next_run_at    INTEGER NOT NULL DEFAULT 0,
    enabled        INTEGER NOT NULL DEFAULT 1,
    created_at     INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000),
    updated_at     INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000)
);

CREATE TABLE cron_runs (
    id             TEXT PRIMARY KEY,
    cron_id        TEXT NOT NULL REFERENCES cron_jobs(id),
    session_id     TEXT NOT NULL REFERENCES sessions(id),
    status         TEXT NOT NULL DEFAULT 'pending'
                   CHECK(status IN ('pending', 'running', 'completed', 'failed')),
    started_at     INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000),
    finished_at    INTEGER,
    exit_code      INTEGER
);
CREATE INDEX idx_cron_runs_cron_id ON cron_runs(cron_id);
