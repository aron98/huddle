-- Issue 27: per-meeting E2EE trust mode. A 0 (default) meeting relays
-- plaintext media through the SFU exactly as before; a 1 meeting is
-- end-to-end encrypted: clients negotiate keys over the signaling relay
-- and (with the transform worker, issue #28) only ciphertext reaches
-- the server. The flag is set at creation time only — trust mode is
-- not flippable afterwards — and recording is refused for E2EE
-- meetings (see the interlock in http.rs, ADR 010).

ALTER TABLE meetings ADD COLUMN e2ee BOOLEAN NOT NULL DEFAULT 0;
