-- Persistent A2A task state: one row per task instance. The daemon's HTTP
-- transport is stateless, but task state itself is durable — resolved tasks
-- stay retrievable via tasks/get, and push notification configs survive
-- restarts.

CREATE TABLE IF NOT EXISTS a2a_tasks (
    id TEXT PRIMARY KEY,
    context_id TEXT NOT NULL,
    state TEXT NOT NULL,
    status_message TEXT,
    response TEXT NOT NULL DEFAULT '',
    completion TEXT,
    artifact_id TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_a2a_tasks_context ON a2a_tasks (context_id);

CREATE TABLE IF NOT EXISTS a2a_push_configs (
    task_id TEXT PRIMARY KEY,
    url TEXT NOT NULL,
    token TEXT,
    auth_scheme TEXT,
    auth_credentials TEXT
);
