-- A2A expansion: ListTasks ordering/pagination (updated_at + turn on every
-- task row), one durable artifact per finished turn, and messageId
-- idempotency (v1.0 §3.3.1 — agents may detect duplicate deliveries).

ALTER TABLE a2a_tasks ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE a2a_tasks ADD COLUMN turn INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS a2a_artifacts (
    task_id TEXT NOT NULL,
    turn INTEGER NOT NULL,
    artifact_id TEXT NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (task_id, turn)
);

CREATE TABLE IF NOT EXISTS a2a_seen_messages (
    message_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_a2a_tasks_updated ON a2a_tasks (updated_at DESC, id DESC);
