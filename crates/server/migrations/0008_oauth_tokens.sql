-- Agent access (issue #9, ADR 013): huddle acts as its own OAuth
-- authorization server with the RFC 8628 device flow.
--
-- `oauth_device_requests` are short-lived polling handles between the
-- CLI and the browser approval page; the row is deleted the moment the
-- grant completes. `oauth_refresh_tokens` are the long-lived client
-- credentials: only the SHA-256 hash of the token is stored, exactly
-- like `login_tokens`, so a leaked database cannot authorize anything.

CREATE TABLE oauth_device_requests (
    id               TEXT PRIMARY KEY,
    device_code_hash TEXT NOT NULL UNIQUE,
    user_code        TEXT NOT NULL,
    client_name      TEXT NOT NULL,
    scopes           TEXT NOT NULL,
    expires_at       TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending',
    organizer_id     TEXT REFERENCES organizers(id),
    last_polled_at   TEXT,
    poll_interval_sec INTEGER NOT NULL DEFAULT 5
);

CREATE INDEX idx_oauth_device_requests_user_code ON oauth_device_requests(user_code);

CREATE TABLE oauth_refresh_tokens (
    id           TEXT PRIMARY KEY,
    hash         TEXT NOT NULL UNIQUE,
    organizer_id TEXT NOT NULL REFERENCES organizers(id),
    client_name  TEXT NOT NULL,
    scopes       TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    last_used_at TEXT
);

CREATE INDEX idx_oauth_refresh_tokens_organizer ON oauth_refresh_tokens(organizer_id);
