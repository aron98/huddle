# ADR 012: Encrypted meeting titles at rest

- Status: proposed (implements issue #29; stacked on ADR 010's key exchange)
- Date: 2026-10-21

## Context

The server operator can read the SQLite file the server writes. The
goal of issue #29 is that this file leaks **no meeting content** — the
operator learns only routing facts: who, when, how to connect. Most of
that already holds: media is end-to-end encrypted in E2EE meetings
(ADR 009/010), E2EE meetings can never record (ADR 010's interlock),
and chat is #28's scope. What remains persistently plaintext today is
the **meeting title**, stored verbatim in `meetings.title` and echoed
into ICS exports.

The central tension: the title is the one piece of content the product
shows **before** a participant holds any key material.

- The **prejoin page** shows the title to someone who has only a join
  code — no identity, no pairwise keys, nothing to decrypt with.
- The **dashboard** shows the organizer their meeting list.
- The **ICS export** embeds the title as `SUMMARY` for calendar
  software that will never speak our protocol.

Deriving the title key from the join code or the passcode is worthless:
the server stores both. Epoch keys (ADR 010) only exist after joining.
Any design must therefore choose *who* can ever see the true title,
and degrade honestly for everyone else.

Candidates considered:

1. **Organizer-scope encryption** — the creating browser seals the
   title to the organizer's own long-term identity key; other clients
   see a masked label until they join, after which the true title
   arrives over the E2EE relay. (Chosen.)
2. **Meeting-scope key distributed after join only** — leaves the
   dashboard and organizer's prejoin blind; the organizer is exactly
   the person who most needs to tell their meetings apart.
3. **Masked everywhere, never decrypt client-side** — wastes the one
   place a decryption key *can* exist (the organizer's browser) and
   makes the dashboard useless.

## Decision 1 — Titles of E2EE meetings are sealed to the organizer's identity key

When an E2EE meeting is created from the dashboard, the browser seals
the plaintext title **before it ever leaves the client**:

- **Key**: the organizer's ADR 010 identity (WebCrypto ECDH P-256,
  non-extractable, in IndexedDB). The title key is
  `HKDF-SHA256(ECDH(own private, own public), salt = "", info =
  "huddle-titles/v1/organizer-key")` → AES-128-GCM key. Self-ECDH
  reuses the pairwise derivation machinery (`derive_pairwise_key`) —
  no new crypto surface.
- **Format**: the existing `title` column stores
  `enc1.` `‖` base64(12-byte nonce ‖ AES-128-GCM ciphertext+tag over
  the UTF-8 title, AAD `"huddle-titles/v1/title"`). The prefix makes
  the row **self-describing**, so no schema change and no migration:
  a row either has the marker or it does not.
- **Validation**: plaintext titles keep the old rules (trimmed,
  non-empty, ≤ 200 chars — validated client-side *before* sealing);
  `enc1.` blobs skip the plaintext shape checks and only get a length
  cap (2048 bytes) — `huddle_protocol::validate_title`.

The client validates the plaintext *first*, then seals, then sends
the blob as the `title` of the create request; the server treats
titles as opaque strings, exactly as before.

### Why E2EE meetings only (a deliberate deviation from "ALL")

The issue recommends encrypting every meeting's title. We scope to
E2EE meetings, for three reasons:

1. **Trust-mode coherence.** A plaintext meeting publishes its media
   and chat to the operator *by explicit choice* — that is what the
   trust mode means (ADR 009). Encrypting only its title protects
   nothing the meeting does not already reveal, while breaking the
   prejoin/ICS story for every guest of every meeting.
2. **There is no distribution channel for plaintext meetings.** The
   keying relay is E2EE-gated by an enforced server rule (ADR 010
   D4), so a title distributed that way can only ever exist for E2EE
   meetings. Masking titles for guests of plaintext meetings would
   change the product for everyone ("Plaintext meetings must not
   break").
3. **Byte-for-byte compatibility.** Plaintext meetings are stored,
   served, exported, and rendered exactly as before — provable in
   tests, not promised.

If a title channel for plaintext meetings is ever added (e.g.
account-scoped keys), switching the default to ALL is a one-line
client change plus this ADR's revision.

**Multi-device cost, honestly stated:** the title key is the creating
browser profile's identity — the organizer's *other* devices see the
masked label, exactly like guests do, until they join. Non-extractable
keys cannot be synced. The upgrade path is account-scoped title keys
registered server-side (a future ADR); this design does not block it.

## Decision 2 — The server ships a masked label, never the plaintext, never a raw blob as UI text

All server→client surfaces mask at the single `From<Meeting> for
MeetingDto>` boundary — REST responses and the WS `Welcome` both flow
through it:

- `title` becomes the constant masked label (`"Encrypted meeting"`),
- and a **new additive field** `title_ciphertext: Option<String>`
  (`#[serde(default, skip_serializing_if = "Option::is_none")]`)
  carries the stored blob for clients that can decrypt.

This is deliberately shaped for compatibility in both directions:

- **Old clients** (pre-`title_ciphertext`) parse the new JSON fine and
  render the clean masked label — they never see a ciphertext blob as
  UI text.
- **New clients** try to decrypt `title_ciphertext` with their own
  identity; the organizer's browser succeeds and shows the true
  title, everyone else falls back to the masked label. A malformed,
  truncated, or foreign-key blob degrades to the label, never to an
  error.

The **ICS export** shows the masked label for encrypted meetings:
calendar software cannot decrypt, and shipping it a blob it would
render as `SUMMARY` is worse than honest degradation. Tracing and
mailers never touch titles (audited below).

## Decision 3 — The true title travels to participants over the E2EE keying relay

After joining an E2EE meeting, a client that knows the true title
(the organizer's browser) shares it with each peer over the existing
`E2eeKeying` relay (ADR 010 D4) as a **new payload tag `0x05`**
(`0x04` is the encrypted-chat envelope tag per ADR 011 D7 — one
byte, one meaning across the protocol family):
12-byte nonce ‖ AES-128-GCM(title, AAD `"huddle-e2ee/v1/title"` ‖
sender id ‖ receiver id), sealed with the *pairwise* key — the same
seal/open machinery as sender keys and verified confirmations. The
server relays it as an opaque payload; no server change at all.

Propagation rules:

- a client that learns the title re-sends it to every peer it can
  seal to (so participants who joined *before* the organizer also get
  it, and late joiners get it from anyone, not just the organizer);
- a client that already knows the title drops redundant copies —
  which is also what prevents relay loops;
- the in-call header updates when the envelope arrives.

If nobody who knows the title ever joins, participants keep the
masked label. That is the honest floor of the design, documented
rather than papered over.

## The honest floor — what the operator still sees

Even with titles sealed, the server operator sees, by design:

- the **roster**: organizer email, who joined when, display names in
  chat rows;
- **join codes** (a capability by design), **passcodes** (a shared
  capability, not a credential — ADR 0006, compared constant-time);
- **scheduling facts**: start/end times, recurrence rules, cancelled
  occurrences;
- **traffic analysis**: packet timing and sizes, and the *length* of
  the encrypted title (padding is out of scope);
- the **fact** that a meeting is E2EE, and its ciphertext blob.

## Persisted-row audit (issue #29's full sweep)

| Table.column | Contents | Operator-readable? | By design? |
|---|---|---|---|
| `organizers.email` | login email | yes | yes — identity, the address the magic link goes to |
| `login_tokens.hash` | SHA-256 of the token | useless | yes — no secret is stored; single-use, expiring |
| `meetings.code` | join code | yes | yes — the join capability itself |
| `meetings.title` | plaintext, or `enc1.` blob for E2EE meetings | plaintext: yes; blob: no | plaintext meetings chose their trust mode; E2EE titles are this ADR |
| `meetings.starts_at/ends_at/created_at` | schedule | yes | yes — routing facts |
| `meetings.rrule/rrule_until/rrule_count` | recurrence | yes | yes — scheduling facts |
| `meetings.organizer_id` | ownership | yes (joins to email) | yes — roster fact |
| `meetings.cancelled` | flag | yes | yes — routing fact |
| `meetings.passcode` | plaintext | yes | yes — shared capability (ADR 0006), not a credential |
| `meetings.e2ee` | trust-mode flag | yes | yes — the participant's own choice, surfaced in the UI |
| `chat_messages.body` | plaintext today | yes | **gap owned by #28** — encrypts bodies for E2EE meetings |
| `chat_messages.sender_id/sender_name/sent_at` | roster | yes | yes — roster facts |
| `oidc_states.hash/verifier/nonce` | PKCE secrets | server-held only | yes — single-use, short-expiry, never leave the server |
| `recordings.*` (id, meeting_id, started_by, times, size) | routing | yes | yes — no media metadata; E2EE meetings can never record (ADR 010 interlock) |
| recording files (`.rtp`, `session.sdp`, `manifest.json`) | track/ssrc mapping | yes | yes — no titles, no content beyond what recording implies |
| `cancelled_occurrences` | schedule | yes | yes — routing facts |

There is no `sessions` table — sessions are stateless HMAC cookies.
Tracing was swept: the only meeting-related log lines are error
statuses (`meeting lookup failed`, `meeting storage failure`) with no
field data; the mailer sends sign-in links only — no titles. Magic
links contain no meeting data.

## Consequences

- **UX**: guests and the organizer's other devices see "Encrypted
  meeting" on prejoin and in ICS for E2EE meetings; the creating
  browser shows the true title on the dashboard, prejoin, and in the
  call; every participant shows the true title in the call header
  once the relay delivers it.
- **No migration.** The `enc1.` marker is self-describing in the
  existing column; rolling back means clients stop sealing.
- **Compat**: old clients render masked labels (never blobs); the
  protocol field is additive-only with serde defaults, so
  `PROTOCOL_VERSION` stays 1.
- **Tests**: server router tests assert ciphertext-at-rest, masked
  API/ICS, and unchanged plaintext meetings; the e2e suite
  (`e2e-titles.mjs`, port 3139) walks the full lifecycle including
  the relay hand-off and reads the SQLite file to prove the
  plaintext never touches disk.
