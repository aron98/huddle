# Single sign-on: setting up an OIDC identity provider

Huddle signs organizers in with passwordless magic links by default. When an
OIDC identity provider (IdP) is configured, SSO **replaces magic links
entirely** — no SMTP transport is needed while SSO is active, and the
magic-link endpoint answers `409` instead of sending mail. Both flows key
organizers by email ([ADR 003](../adr/003-passwordless-identity-trait.md)),
so the same person lands in the same account — with the same meeting
ownership — whichever flow they used before or after the switch.

This guide is the step-by-step version of the
[README's SSO section](../../README.md#single-sign-on-oidc): first the
generic IdP contract any provider must satisfy, then a walk-through with the
exact Auth0 pitfalls we hit during a live production deployment on
2026-10-22. The [diagnostics table](#diagnostics) maps each server log line
to its cause and fix — work to make these errors self-explanatory at the API
surface is tracked in [#88](https://github.com/aron98/huddle/issues/88).

## What huddle expects from the provider

Huddle speaks the OIDC authorization-code flow with PKCE (`S256`), a
`state`, and a `nonce` — all generated and validated by huddle itself, so
there is nothing to configure on the huddle side beyond the three values
below. Your IdP application must satisfy:

| Requirement | Value |
|---|---|
| Application type | A confidential "Regular Web Application"-style client (server-side, authorization-code flow) |
| Redirect (callback) URL | `<HUDDLE_BASE_URL>/api/auth/oidc/callback` — e.g. `https://meet.example.com/api/auth/oidc/callback` |
| Scopes | huddle always requests `openid email`; **no `audience` parameter is sent**, and none is needed |
| Token-endpoint authentication | `client_secret_post` — huddle sends `client_id` + `client_secret` in the POST body |
| ID token claims | Must carry `email` and **`email_verified: true`** — huddle is strict about this, because organizer identity *is* the email; a missing/false claim fails the login |
| ID token format | A signed JWS (three dot-separated segments). JWE-encrypted tokens cannot be verified (see the [Auth0 gotchas](#auth0-walk-through)) |
| Signature algorithm | Asymmetric, verified against the issuer's JWKS. RS256 is the tested configuration; HS256 can never work (huddle never uses the client secret as a signing key and skips octet keys in the JWKS) |
| Discovery | The issuer must publish `authorization_endpoint`, `token_endpoint`, and `jwks_uri` at `<issuer_url>/.well-known/openid-configuration` |

Huddle reads all endpoints from the discovery document — no per-endpoint
configuration exists.

## Configuring huddle

Either an `[oidc]` section in `huddle.toml`:

```toml
[oidc]
issuer_url = "https://tenant.eu.auth0.com"   # bare https:// URL, no trailing slash
client_id = "…"
client_secret = "…"
```

…or the three `HUDDLE_OIDC_*` environment variables:

```sh
HUDDLE_OIDC_ISSUER_URL=https://tenant.eu.auth0.com
HUDDLE_OIDC_CLIENT_ID=…
HUDDLE_OIDC_CLIENT_SECRET=…
```

**All three or none.** If any of the three variables is set, all three must
end up non-empty — a half-configured OIDC fails loudly at startup instead of
silently falling back to magic links. Each variable overrides the
corresponding `[oidc]` TOML field.

**Issuer URL rules** (most of the failure modes below live here):

- It is the IdP's *bare* `https://` origin (plus realm/path where your IdP
  uses one) — **no trailing slash**: `https://tenant.eu.auth0.com`.
- Huddle trims a trailing slash before building the discovery URL
  (`<issuer>/.well-known/openid-configuration`). The trim matters: with a
  literal slash the URL would contain `//` and the IdP would answer `404`.
  Write it without the slash anyway — every comparison below is made after
  slash-trimming, so the trimmed value is the identity you are claiming.
- The trimmed value must match the issuer your IdP *advertises* — in its
  discovery document and in the ID token's `iss` claim. A mismatch is
  rejected (mix-up defense).

In a `docker-compose.yml` environment list, take the values from a
`.env` file or write them bare — see gotcha 1 below before quoting anything.

```yaml
    environment:
      # List form: everything after `=` is taken LITERALLY — no quotes.
      - HUDDLE_OIDC_ISSUER_URL=https://tenant.eu.auth0.com
      - HUDDLE_OIDC_CLIENT_ID=${AUTH0_CLIENT_ID}
      - HUDDLE_OIDC_CLIENT_SECRET=${AUTH0_CLIENT_SECRET}
```

## Auth0 walk-through

1. **Create the application.** Auth0 Dashboard → Applications →
   *Create Application* → **Regular Web Application**. From its *Settings*
   tab note the **Domain**, **Client ID**, and **Client Secret** — they map
   to `issuer_url` (`https://<Domain>`), `client_id`, and `client_secret`.
2. **Allowed Callback URLs** (Settings tab): enter
   `https://meet.example.com/api/auth/oidc/callback` (your
   `HUDDLE_BASE_URL` + `/api/auth/oidc/callback`). A wrong or missing entry
   fails at the IdP with `callback URL mismatch` *before* huddle is ever
   reached.
3. **Signature algorithm.** Advanced Settings → OAuth →
   **JSON Web Token Signature Algorithm: `RS256`** (the default). See
   gotcha 5 for why HS256 cannot work.
4. **Client authentication.** In the application's authentication-method
   settings, pick **Client Secret (Post)** — this matches huddle's
   `client_secret_post`; see gotcha 6.
5. **Point huddle at it.** Configure the three values (section above),
   restart huddle, and load the site — opening the sign-in page sends
   you straight to Auth0 ([below](#the-login-flow)); on older releases
   open `https://meet.example.com/api/auth/oidc/login` manually.

### Gotchas — all of these bit us live

**1. Compose list-form env values keep their quotes.** In YAML list-form
`environment:` entries, everything after `=` is passed to the container
verbatim. This is wrong:

```yaml
      # WRONG — the container sees the literal quotes in the value
      - HUDDLE_OIDC_ISSUER_URL="https://tenant.eu.auth0.com"
```

…while this is right:

```yaml
      - HUDDLE_OIDC_ISSUER_URL=https://tenant.eu.auth0.com
```

The quoted value makes the issuer `"https://…"` — not a URL — and the
discovery request fails to build (`builder error`), which the browser sees
as a `502` (see [diagnostics](#diagnostics)). Map-form entries
(`HUDDLE_OIDC_ISSUER_URL: "https://…"`) are fine: YAML itself strips the
quotes there.

**2. A missing `https://` scheme fails the same way.**
`HUDDLE_OIDC_ISSUER_URL=tenant.eu.auth0.com` is not a URL either and
produces the identical `builder error` → `502`. The issuer must include the
scheme.

**3. Auth0 may issue JWE-*encrypted* ID tokens — and hide that it does.**
If an *encryption key* is configured on the application, Auth0 encrypts the
ID token (header `{"alg":"dir","enc":"A256GCM"}`, five dot-separated
segments). Huddle can only verify *signed* JWS tokens and fails with a
confusing `ID token rejected: InvalidSignature`. The current dashboard UI
does not show the encryption key — remove it through the Management API:

```sh
# inspect (the field is not visible in the dashboard):
curl -sS -H "Authorization: Bearer $MGMT_TOKEN" \
  "https://$TENANT/api/v2/clients/$CLIENT_ID" | jq .encryption_key
# remove it:
curl -sS -X PATCH -H "Authorization: Bearer $MGMT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"encryption_key": null}' \
  "https://$TENANT/api/v2/clients/$CLIENT_ID"
```

The Management API token needs the `read:clients` and `update:clients`
scopes. Note: the community advice about setting an **audience** (API
identifier / "Default Audience") concerns *access* tokens, not the ID token
huddle consumes — huddle sends no audience parameter, and no audience
setting fixes this.

**4. Auth0's issuer carries a trailing slash.** Auth0 advertises
`https://tenant.eu.auth0.com/` — with the slash — in discovery and in
`iss`, even though you configure huddle without one. Huddle v1.1.3+
normalizes both sides before comparing; on older releases every login fails
with `ID token rejected: InvalidIssuer`, and no configuration works around
it (adding the slash does not help — huddle trims it again before
comparing). **Upgrade to v1.1.3 or later.**

**5. The signature algorithm must be RS256.** Huddle verifies ID tokens
only against the issuer's JWKS public keys and skips octet keys outright, so
Auth0's HS256 option can never succeed — it fails with `InvalidSignature`.
RS256 is Auth0's default and the tested configuration.

**6. Client authentication must be Client Secret (Post).** Huddle
presents its credentials as `client_secret_post` (in the token-endpoint
POST body). Select **Client Secret (Post)** among Auth0's client
authentication methods so the token exchange matches.

## The login flow

- **Current releases:** with SSO configured, the login page probes
  `GET /api/auth/oidc/status` when it loads and, on
  `{"status":"oidc_configured",…}`, sends the browser straight to
  `/api/auth/oidc/login` — the user never sees the magic-link form
  ([#99](https://github.com/aron98/huddle/issues/99)). The probe is
  best-effort: when it cannot run, the behavior below takes over, so
  the page always offers a way in.
- **v1.1.3:** the login page shows the magic-link form; submitting it
  gets `409 {"status":"oidc_configured","login":"/api/auth/oidc/login"}`
  from the server and the page follows that URL automatically. This
  path is still in place as the fallback for a failed probe.
- **Older releases:** the login page shows the raw error instead; open
  `https://meet.example.com/api/auth/oidc/login` manually.
- The IdP redirects back to `/api/auth/oidc/callback`, huddle verifies the
  code (PKCE, state, nonce, signature, issuer) and lands the user on
  `/meetings` with the same signed session cookie a magic link issues.
  `/api/auth/me` and sign-out behave identically in both flows.
- The browser-side login state is single-use and expires after **10
  minutes** — an idle half-finished IdP login is rejected as expired.
  Restart from the huddle login page.

## Diagnostics

The server log is the first stop — `docker compose logs huddle`
([compose guide](docker-compose.md)), `journalctl -u huddle`
([VPS guide](ubuntu-vps.md)), or your platform's equivalent.

| Log line | Cause | Fix |
|---|---|---|
| `cannot start OIDC login: … fetching discovery document: builder error` (browser gets `502`) | Malformed issuer URL: literal quotes from a compose list-form entry (gotcha 1), missing `https://` scheme (gotcha 2), or stray whitespace | Fix `HUDDLE_OIDC_ISSUER_URL`; verify what the container actually received with `docker compose exec huddle env` |
| `OIDC callback rejected: ID token rejected: InvalidSignature` | The ID token is not a plain RS256 JWS: JWE-encrypted (gotcha 3), HS256-signed (gotcha 5), or the JWKS is unreachable/stale | Remove the encryption key / set RS256 / check the JWKS URL from discovery |
| `OIDC callback rejected: ID token rejected: InvalidIssuer` | Issuer mismatch on huddle < v1.1.3 — Auth0's trailing-slash issuer (gotcha 4) | Upgrade to v1.1.3+ |
| `OIDC callback rejected: ID token issuer … does not match …` (v1.1.3+) | The configured issuer differs from the advertised one even after slash-trimming | Align `HUDDLE_OIDC_ISSUER_URL` with the `iss` your IdP actually emits |
| Browser gets `401 … "OIDC sign-in failed or has expired"` | State/nonce/expiry: login state older than 10 minutes or already used (back-button replay), callback in a different browser/cookie jar, or the ID token lacks `email_verified: true` | Restart the login from the huddle page; if it persists, decode the ID token and check its claims |

If the very first redirect never happens, the problem is issuer config
(row 1). If the IdP screen works but the return trip fails, it is the
callback validation (the other rows).

## Related

- [README: single sign-on (OIDC)](../../README.md#single-sign-on-oidc) — the configuration reference
- [ADR 003](../adr/003-passwordless-identity-trait.md) — passwordless identity design
- [#88](https://github.com/aron98/huddle/issues/88) — making these failures self-explanatory at the API surface
- Deployment guides: [Ubuntu VPS](ubuntu-vps.md) · [Docker Compose](docker-compose.md) · [Kubernetes](kubernetes.md) · [Hardening checklist](hardening-checklist.md)
