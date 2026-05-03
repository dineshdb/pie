CREATE TABLE steps (
    id          INTEGER PRIMARY KEY,
    session_id  TEXT    NOT NULL REFERENCES sessions(id),
    name        TEXT    NOT NULL,
    status      TEXT    NOT NULL, -- pending, in_progress, completed, failed, skipped
    created_at  INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000),
    UNIQUE(session_id, name)
);
CREATE INDEX idx_steps_session_id ON steps(session_id);

ALTER TABLE messages ADD COLUMN hash TEXT;
ALTER TABLE messages ADD COLUMN fail_hash TEXT;
