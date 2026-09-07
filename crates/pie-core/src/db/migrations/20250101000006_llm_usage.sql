-- LLM usage per run: token counts, cache hits and cost for bookkeeping.
-- `cost_usd` is NULL when the model has no [pricing.*] entry in the config.
CREATE TABLE llm_usage (
    id                INTEGER PRIMARY KEY,
    session_id        TEXT    NOT NULL REFERENCES sessions(id),
    ts                INTEGER NOT NULL,
    model             TEXT    NOT NULL,
    agent             TEXT,
    requests          INTEGER NOT NULL,
    prompt_tokens     INTEGER NOT NULL,
    completion_tokens INTEGER NOT NULL,
    cached_tokens     INTEGER NOT NULL,
    reasoning_tokens  INTEGER NOT NULL,
    total_tokens      INTEGER NOT NULL,
    cost_usd          REAL
);
CREATE INDEX idx_llm_usage_session_id ON llm_usage(session_id);
CREATE INDEX idx_llm_usage_ts ON llm_usage(ts);
