-- huddle initial schema (see docs/ARCHITECTURE.md -> Data model).
-- Times are RFC3339 TEXT in UTC. Ids are UUIDv4 TEXT.

CREATE TABLE organizers (
    id         TEXT PRIMARY KEY,
    email      TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL
);

CREATE TABLE login_tokens (
    hash       TEXT PRIMARY KEY,
    expires_at TEXT NOT NULL
);

CREATE TABLE meetings (
    id           TEXT PRIMARY KEY,
    code         TEXT NOT NULL UNIQUE,
    title        TEXT NOT NULL,
    starts_at    TEXT NOT NULL,
    ends_at      TEXT NOT NULL,
    organizer_id TEXT REFERENCES organizers(id),
    cancelled    INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL
);

CREATE INDEX idx_meetings_code ON meetings(code);

CREATE TABLE chat_messages (
    id          TEXT PRIMARY KEY,
    meeting_id  TEXT NOT NULL REFERENCES meetings(id),
    sender_id   TEXT,
    sender_name TEXT NOT NULL,
    body        TEXT NOT NULL,
    sent_at     TEXT NOT NULL
);

CREATE INDEX idx_chat_meeting_time ON chat_messages(meeting_id, sent_at);
