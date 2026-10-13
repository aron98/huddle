# ADR 003: Passwordless organizer + anonymous guests, behind an IdentityProvider trait

- Status: accepted
- Date: 2026-10-13

## Context

Scheduled meetings need an owner, but self-hosters must not be forced to run a
password database, and guests must never be asked to register. The user has
stated that the first post-MVP feature will be **OIDC/SAML integration** for
company SSO. Whatever we choose must make that a new implementation, not a rewrite.

## Decision

- **Organizers** log in via **magic link** (email → one-time token → short-lived
  session cookie). Token hashes stored in SQLite, short TTL, single-use.
- **Guests** are anonymous: display name only. The meeting link itself is the
  capability.
- All auth flows sit behind an `IdentityProvider` trait in `server`; the magic-link
  flow is its first implementation.

## Consequences

**Positive**
- No password storage, reset flows, or password-policy surface for self-hosters.
- Zero-friction guest join (Jitsi-style) — critical for adoption.
- OIDC/SAML slots in as a second `IdentityProvider` impl: verify the SSO callback,
  issue the same session cookie. Callers never change.
- Anonymous guests never touch the trait, keeping it minimal.

**Negative**
- Requires outbound email from the self-hosted server (SMTP config in env/config
  file) — document a provider-agnostic `lettre` setup, and allow disabling auth
  entirely for single-user test deployments.
- Magic links are phishing-prone like all passwordless flows; mitigate with short
  TTLs and single-use hashes.
- Per-meeting passcodes (defense for leaked links) deferred to post-MVP.
