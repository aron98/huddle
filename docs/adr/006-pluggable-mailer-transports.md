# ADR 006: Pluggable mail delivery behind a MailTransport trait

- Status: accepted
- Date: 2026-10-20
- Relates to: [ADR 003](003-passwordless-identity-trait.md), issue #35

## Context

Magic-link delivery was hardcoded to two modes in `mailer.rs`: a free
function `send_login_link` that logged the link when `[smtp]` was absent and
sent through a single lettre STARTTLS relay when it was present. Email use
is growing past login (meeting invites, reminders are on the roadmap), and
operating a relay is not the only reasonable delivery option — HTTP email
APIs such as Resend (and, later, SendGrid/Mailgun/SES/Postmark) remove the
SMTP operations burden from small self-hosters. There was no way to plug in
a second transport without editing `mailer.rs` and every caller.

ADR 003 faced the same shape of problem for login flows and set the pattern:
a trait the rest of the server codes against, config-time selection of a
concrete implementation. This ADR applies it to email.

## Decision

- **Trait, not functions.** `MailTransport` is the boundary callers code
  against; its one method `send(Email)` is async-fn-in-trait. The signature
  carries the message (`Email { to, subject, text }`) — never
  provider-specific knobs — so new providers cannot leak into callers.
- **Concrete enum, no trait objects.** Exactly as with `IdentityProvider`,
  an async-fn-in-trait is not dyn-safe. Configuration therefore selects a
  concrete transport at startup via the `Mailer` enum (`Log | Smtp | Resend`)
  implementing the trait by delegation. Adding a provider means adding a
  type plus one arm in the enum (and one in config) — nothing else changes.
- **Three transports.**
  - `log` (default): the link is written to the server log at `info`.
    Evaluating huddle must never require an email account.
  - `smtp`: the existing lettre STARTTLS path, preserved verbatim.
  - `resend`: one `POST https://api.resend.com/emails` call with
    `Authorization: Bearer <api key>` and a JSON body — a thin `reqwest`
    call (reqwest is already a dependency for OIDC), no SDK, keeping the
    binary small and the wire format auditable.
- **Config shape: explicit selection with detection as the default.**
  A `[mail]` section with `provider = "log" | "smtp" | "resend"` pins the
  transport and always wins; per-provider settings live in sub-tables
  (`[mail.resend]` with `api_key` and `from`). When `provider` is omitted,
  the transport is detected from whichever section is present: nothing
  configured logs the email (dependency-free default), a bare `[smtp]`
  selects SMTP — **existing deployments keep working unchanged** — and
  `[mail.resend]` selects Resend. Both `[smtp]` and `[mail.resend]`
  present without an explicit `provider` is a startup error, not a silent
  pick. SMTP knobs stay in the legacy top-level `[smtp]` section only:
  it doubles as `provider = "smtp"`'s parameters, so there is one home for
  them and no duplicated settings.
- **Environment.** Figment's flat env keys cannot reach nested sections
  (see the OIDC handling in `config.rs`), so `HUDDLE_MAIL_PROVIDER`,
  `HUDDLE_MAIL_RESEND_API_KEY` / `HUDDLE_MAIL_RESEND_FROM`, and
  `HUDDLE_SMTP_*` are applied explicitly after extraction, mirroring the
  `HUDDLE_OIDC_*` precedent — including failing loudly on
  half-configured transports.
- **Results stay observable.** `MailResult` grows one variant per
  transport (`Logged`, `SmtpSent`, `ResendSent`) so tests and logs can
  assert the path taken. Resend is unit-tested against an in-process mock
  of the API; tests never contact the live service.

Selection and validation live in `Config::mail_selection()` as pure data
(`MailSelection`) so config-level tests cover every combination without
building clients; `Mailer::select` only turns the selection into the
concrete enum.

## Consequences

**Positive**

- A new provider is one type + two arms (config, enum); callers never
  change, and the identity flow gains a provider-agnostic `Email` it can
  reuse for future meeting invites.
- Self-hosters without any email account keep a first-class log mode;
  small production setups get a hosted-API option without running SMTP.
- Pre-issue-35 configs behave identically: a lone `[smtp]` section still
  selects the relay, and no section still logs the link.

**Negative**

- The enum must be touched for every new provider (match arms are the
  price of not being dyn-safe — accepted, as in ADR 003).
- No runtime switching or plugin transports; provider choice is fixed at
  config load, which is also what keeps validation strict and early.
- Two config mechanisms coexist (explicit `provider`, presence
  detection). Detection exists solely so legacy `[smtp]`-only files keep
  working; it errors rather than guessing when ambiguous.

## Amendment

- Future providers (SendGrid, Mailgun, SES, Postmark) should follow the
  Resend template: a thin reqwest call, a `[mail.<provider>]` sub-table,
  and an arm in `Mailer`/`MailSelection`.
