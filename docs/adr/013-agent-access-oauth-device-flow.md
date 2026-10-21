# ADR 013: Agent access — huddle as its own OAuth authorization server

- Status: accepted
- Date: 2026-10-21

## Context

Scheduled meetings are only half the story if the only client is a browser:
CI jobs, scripts, and AI agents need to create and list meetings without
driving a UI. Session cookies are unusable outside a browser, passwords do
not exist in huddle (ADR 003), and delegating to an external authorization
server would force self-hosters to run one just to script their meeting bot.
The user-facing approval surface must be the dashboard people already trust,
and a leaked or stale credential must be revocable from that same surface.

## Decision

- **huddle is its own OAuth authorization server.** It implements the RFC 8628
  device authorization grant (`POST /api/oauth/device/register`, browser
  approval at `/oauth/device`, token polling at `POST /api/oauth/token`) plus
  the RFC 6749 refresh-token grant and an RFC 7009-style revocation endpoint
  (`POST /api/oauth/revoke`). The register/token endpoints are form-encoded
  per the RFCs and rate-limited per IP like magic links.
- **The approval page is server-rendered plain HTML**, no scripts, CSP
  `default-src 'none'`. It requires the browser session cookie, shows the
  client name and the exact scopes requested, and guards its POST forms with a
  stateless HMAC CSRF token bound to (session key, organizer, user code). No
  Leptos/trunk involvement — this page must work regardless of the web build.
- **Scopes are `meetings:read` and `meetings:write`**, enforced server-side on
  the bearer path. `write` does not imply `read`. A read-only token can never
  degrade into an anonymous create; it gets `403`.
- **Bearer access is a second request principal**, alongside the session
  cookie: cookie sessions keep full power, bearer requests are checked against
  the scopes embedded in their token. `/api/auth/me` accepts both so a CLI can
  say who it acts as.
- **Token model:**
  - *Access tokens* are stateless, HMAC-signed strings shaped like session
    cookies (`at1.<organizer_id>.<scopes>.<unix-expiry>.<hmac>`), signed with
    the server's session key, TTL 30 minutes. Note: uniqueness is
    second-granular — two mints for the same organizer+scopes in the same
    second are byte-identical, which is harmless (the token carries no
    per-request entropy and expires anyway).
  - *Refresh tokens* are 32 random bytes, sha256-**hashed at rest** in SQLite
    (same discipline as magic-link tokens), long-lived (90 days), revocable.
    The refresh grant does not rotate the token; it updates `last_used_at`.
  - *Device codes* are stored hashed; user codes are 8 chars from an unambiguous
    alphabet (`XXXX-XXXX`).
- **Revocation is a hash-row delete.** The dashboard lists authorizations
  (`GET /api/oauth/authorizations`, session-only) and revoke buttons issue
  `DELETE /api/oauth/authorizations/{id}`. A revoked refresh token fails the
  next refresh immediately; the stateless access token lapses within its TTL —
  the same trade-off ADR 003 makes for session cookies.
- **Layering: `crates/cli` (binary `huddle-cli`) is a thin REST wrapper.** It
  speaks the device flow (`huddle-cli login`), stores the refresh token
  (chmod 0600 JSON file at `~/.config/huddle/cli.json`, overridable with
  `--token-file` / `$HUDDLE_CLI_TOKEN_FILE`), refreshes transparently on 401,
  and exposes `--json` on every command with exit codes 0 ok / 1 usage /
  2 auth / 3 server error. `huddle-cli mcp` serves the MCP tools over stdio
  JSON-RPC from the same binary and the same login — no separate crate, no
  separate credential.

## Consequences

**Positive**
- One login serves scripts, CLIs, and MCP-capable agents; approval happens in
  the browser the organizer already trusts, with the exact scopes on screen.
- Scopes give least-privilege automation; the dashboard makes authorization a
  visible, revocable object rather than an invisible forever-token.
- Refresh tokens are useless to a database thief (hashes only); access tokens
  are useless after 30 minutes even if never revoked.
- No new infrastructure for self-hosters — the authorization server is the
  meeting server, one SQLite file richer (migration 0008).

**Negative / accepted trade-offs**
- Refresh-token storage is a plain file with 0600, not an OS keyring —
  deferred as out of scope for day one; the file location is overridable and
  the format is trivially re-importable when a keyring backend lands.
- Revocation cannot kill an unexpired access token; it bounds the damage to
  the access-token TTL.
- The MCP server is hand-rolled newline-delimited JSON-RPC rather than an
  `rmcp` dependency, keeping the CLI crate dependency-light; it implements the
  small subset agents need (initialize, tools/list, tools/call) and must track
  MCP spec evolution manually.
- `POST /api/oauth/revoke` is not rate-limited (cheap single-row delete;
  revocation must never be DoS-able into unavailability for the owner).
- slow_down pacing is enforced only while a request is pending — an approved
  grant completes regardless of poll interval (slow_down is a variant of
  authorization_pending, so this stays RFC-compatible and makes flows
  deterministic without sleeps).
- The device grant is claimed exactly-once via a `DELETE … WHERE id` with a
  rows-affected check; concurrent polls cannot double-mint refresh tokens.
- Recording routes accept a write-scoped bearer token (owner-checked), but
  `started_by` stays `None` for bearer requests — the principal is a tokened
  agent, not a logged-in organizer.
- Bearer requests that pass scope checks still need the organizer to exist;
  deleting an organizer account orphans their tokens (no cascade revocation
  today — refresh still grants, but every resulting access token is rejected
  because the organizer row is gone).
