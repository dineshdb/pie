CREATE TABLE steps (
    id          INTEGER PRIMARY KEY,
    session_id  TEXT    NOT NULL REFERENCES sessions(id),
    name        TEXT    NOT NULL,
    status      TEXT    NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000),
    UNIQUE(session_id, name)
);
CREATE INDEX idx_steps_session_id ON steps(session_id);

-- Auto-touch updated_at on message insert
CREATE TRIGGER trg_messages_touch_session
AFTER INSERT ON messages
BEGIN
    UPDATE sessions SET updated_at = unixepoch('subsec') * 1000 WHERE id = NEW.session_id;
END;
