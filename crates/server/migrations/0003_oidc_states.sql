-- OIDC login states (issue #11): the counterpart of magic-link tokens
-- for the OIDC provider. The `state` value is what travels through the
-- browser and the IdP redirect; the PKCE verifier and nonce it maps to
-- never leave the server. Single-use like a magic token: the callback
-- claims the row atomically before talking to the IdP.
CREATE TABLE oidc_states (
    hash       TEXT PRIMARY KEY,
    verifier   TEXT NOT NULL,
    nonce      TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    used       INTEGER NOT NULL DEFAULT 0
);
