CREATE TABLE sessions (
    id          TEXT PRIMARY KEY,
    cwd        TEXT NOT NULL DEFAULT '',
    title     TEXT NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000)
);

CREATE TABLE messages (
    id          INTEGER PRIMARY KEY,
    session_id  TEXT    NOT NULL REFERENCES sessions(id),
    ts          INTEGER NOT NULL,
    role        TEXT    NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    content     TEXT    NOT NULL,
    compacted   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_messages_session_id ON messages(session_id);
