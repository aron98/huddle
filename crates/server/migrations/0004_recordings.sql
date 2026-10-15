-- Issue 15: meeting recordings. One row per recording session, written
-- by the REST start endpoint and finalized (stopped_at + size_bytes) by
-- the recorder writer thread when the recording stops — explicitly via
-- the stop endpoint, or implicitly when the meeting empties and the
-- recorder is dropped. UUIDs are TEXT, times are RFC3339 TEXT, matching
-- the rest of the schema.

CREATE TABLE recordings (
    id         TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL REFERENCES meetings(id),
    started_by TEXT,
    started_at TEXT NOT NULL,
    stopped_at TEXT,
    size_bytes INTEGER
);

CREATE INDEX idx_recordings_meeting ON recordings(meeting_id);
