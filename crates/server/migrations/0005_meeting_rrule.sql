-- Recurring meetings (issue 16).
--
-- A series stores its curated rule in RFC 5545 fragment form
-- (`FREQ=DAILY`, `FREQ=WEEKLY`, `FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR`,
-- `FREQ=MONTHLY`) plus the optional series bound. Occurrence starts
-- are never materialized: they are expanded from the rule at join and
-- dashboard time, so `starts_at` keeps meaning "the first occurrence".

ALTER TABLE meetings ADD COLUMN rrule TEXT;
ALTER TABLE meetings ADD COLUMN rrule_until TEXT;
ALTER TABLE meetings ADD COLUMN rrule_count INTEGER;

-- Per-occurrence cancellations: cancelling one date of a series
-- leaves the series (and every other occurrence) intact.
CREATE TABLE cancelled_occurrences (
    meeting_id TEXT NOT NULL REFERENCES meetings(id),
    starts_at TEXT NOT NULL,
    PRIMARY KEY (meeting_id, starts_at)
);
