-- Magic-link login (ADR 003): tokens map to an organizer and are
-- single-use. The 0001 shape (bare hash + expiry) predates the organizer
-- association; the schema has not shipped, so the table is recreated.

DROP TABLE IF EXISTS login_tokens;

CREATE TABLE login_tokens (
    hash         TEXT PRIMARY KEY,
    organizer_id TEXT NOT NULL REFERENCES organizers(id),
    expires_at   TEXT NOT NULL,
    used         INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_login_tokens_organizer ON login_tokens(organizer_id);
