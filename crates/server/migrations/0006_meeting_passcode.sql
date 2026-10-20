-- Issue 13: per-meeting passcodes. A NULL passcode means the meeting
-- is open to anyone holding the link (the default, unchanged behavior);
-- a non-NULL value must be supplied on the WS join and is compared in
-- constant time. Stored in plain text: it is a shared capability like
-- the meeting code itself, not a credential we need to one-way hash.

ALTER TABLE meetings ADD COLUMN passcode TEXT;
