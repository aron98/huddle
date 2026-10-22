//! Organizer identity (ADR 003): passwordless login.
//!
//! [`IdentityProvider`] is the boundary the rest of the server codes
//! against. Two implementations exist: [`MagicLinkIdentityProvider`]
//! (the MVP — single-use tokens whose SHA-256 hashes live in
//! `login_tokens`) and [`OidcIdentityProvider`] (issue #11 — SSO via an
//! OIDC issuer). The async trait is not dyn-safe, so config selects a
//! concrete provider at startup via the [`Identity`] enum; callers
//! dispatch through the trait either way.
//!
//! Both flows key organizers by email, so a user can sign in with a
//! magic link today and SSO tomorrow and land in the same account.

use base64::Engine as _;
use rand::RngExt;
use sha2::{Digest, Sha256};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::config::OidcConfig;
use crate::meetings::{from_text, to_text};
/// A verified organizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organizer {
    pub id: Uuid,
    pub email: String,
}

/// Identity flow failures.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("invalid or expired login token")]
    InvalidToken,
    #[error("invalid email address")]
    InvalidEmail,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
    #[error("corrupt identity data: {0}")]
    Corrupt(String),
    #[error("identity provider unreachable or misbehaving: {0}")]
    Provider(String),
}

/// How organizers prove who they are (ADR 003). Guests never touch this.
pub trait IdentityProvider {
    /// Begin a login for `email`, creating the organizer on first use.
    ///
    /// Magic links mint a one-time token here; the caller builds the
    /// verify URL and delivers it (see `mailer`). OIDC builds the full
    /// authorization URL — `email` is only a `login_hint` when it
    /// happens to be a valid address — and the browser is redirected
    /// there by the caller.
    async fn start_login(&self, email: &str, now: OffsetDateTime) -> Result<String, IdentityError>;

    /// Exchange a one-time capability for its organizer. Tokens and
    /// OIDC states are single-use: a second call with the same value
    /// must fail.
    ///
    /// `state` is the OIDC `state` parameter (`None` for magic links);
    /// `token` is the magic token or, for OIDC, the authorization code.
    async fn verify(
        &self,
        token: &str,
        state: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<Organizer, IdentityError>;
}

/// The MVP [`IdentityProvider`]: one-time tokens backed by SQLite.
pub struct MagicLinkIdentityProvider {
    pool: sqlx::SqlitePool,
    /// How long a fresh login token stays valid (ADR 003: short).
    ttl: time::Duration,
}

impl MagicLinkIdentityProvider {
    pub fn new(pool: sqlx::SqlitePool, ttl: time::Duration) -> Self {
        Self { pool, ttl }
    }
}

impl IdentityProvider for MagicLinkIdentityProvider {
    async fn start_login(&self, email: &str, now: OffsetDateTime) -> Result<String, IdentityError> {
        let email = email.trim();
        if !valid_email(email) {
            return Err(IdentityError::InvalidEmail);
        }

        let organizer_id = ensure_organizer(&self.pool, email, now).await?;

        // 32 random bytes, hex-encoded: 64 URL-safe characters. Only the
        // SHA-256 hash is stored, so a leaked database cannot log anyone
        // in.
        let token: [u8; 32] = rand::rng().random();
        let token = hex(&token);
        sqlx::query("INSERT INTO login_tokens (hash, organizer_id, expires_at, used) VALUES (?1, ?2, ?3, 0)")
            .bind(hash_token(&token))
            .bind(organizer_id.to_string())
            .bind(to_text(now + self.ttl).map_err(|e| IdentityError::Corrupt(e.to_string()))?)
            .execute(&self.pool)
            .await?;
        Ok(token)
    }

    async fn verify(
        &self,
        token: &str,
        _state: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<Organizer, IdentityError> {
        let token = token.trim();
        let row = sqlx::query(
            "SELECT t.used, t.expires_at, o.id, o.email \
             FROM login_tokens t JOIN organizers o ON o.id = t.organizer_id \
             WHERE t.hash = ?1",
        )
        .bind(hash_token(token))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(IdentityError::InvalidToken)?;

        let used: i64 = row
            .try_get("used")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        let expires_at: String = row
            .try_get("expires_at")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        if used != 0 {
            return Err(IdentityError::InvalidToken);
        }
        let expires_at = from_text(&expires_at, "expires_at")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        if expires_at <= now {
            return Err(IdentityError::InvalidToken);
        }

        // Mark single-use first, atomically: a concurrent verify racing us
        // loses on the `used = 0` guard.
        let claimed = sqlx::query("UPDATE login_tokens SET used = 1 WHERE hash = ?1 AND used = 0")
            .bind(hash_token(token))
            .execute(&self.pool)
            .await?;
        if claimed.rows_affected() != 1 {
            return Err(IdentityError::InvalidToken);
        }

        let id: String = row
            .try_get("id")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        let email: String = row
            .try_get("email")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        Ok(Organizer {
            id: Uuid::parse_str(&id).map_err(|e| IdentityError::Corrupt(e.to_string()))?,
            email,
        })
    }
}

/// Create the organizer on first login, return its id either way.
async fn ensure_organizer(
    pool: &sqlx::SqlitePool,
    email: &str,
    now: OffsetDateTime,
) -> Result<Uuid, IdentityError> {
    sqlx::query("INSERT INTO organizers (id, email, created_at) VALUES (?1, ?2, ?3) ON CONFLICT (email) DO NOTHING")
        .bind(Uuid::new_v4().to_string())
        .bind(email)
        .bind(to_text(now).map_err(|e| IdentityError::Corrupt(e.to_string()))?)
        .execute(pool)
        .await?;
    let id: String = sqlx::query("SELECT id FROM organizers WHERE email = ?1")
        .bind(email)
        .fetch_one(pool)
        .await?
        .try_get("id")
        .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
    Uuid::parse_str(&id).map_err(|e| IdentityError::Corrupt(e.to_string()))
}

/// Deliberately light: an RFC 5322 parser is not worth the dependency.
/// Must be non-empty, contain `@` with non-empty local and domain parts,
/// and stay within the 254-character limit.
fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && email.len() <= 254
        && !email.chars().any(char::is_whitespace)
}

fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ---------------------------------------------------------------------------
// OIDC single sign-on (issue #11)
// ---------------------------------------------------------------------------

/// The OAuth2/OIDC endpoints the issuer advertises at discovery.
#[derive(Debug, Clone, serde::Deserialize)]
struct Discovery {
    #[serde(default)]
    issuer: Option<String>,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// The ID-token claims huddle relies on. `aud`/`exp` are checked by the
/// JWT library; `iss` is validated by us, normalized for a trailing `/`
/// (issuers such as Auth0 emit `https://tenant/` even when configured
/// without the slash); everything here is checked by us.
#[derive(Debug, serde::Deserialize)]
struct IdTokenClaims {
    #[allow(dead_code)] // presence is required; value is the organizer key
    sub: String,
    iss: String,
    email: String,
    email_verified: Option<bool>,
    nonce: Option<String>,
}

/// SSO organizer login against an OIDC issuer.
///
/// The authorization-code flow with PKCE (S256) and a `state` + `nonce`
/// pair bound server-side. `oidc_states` rows play the role magic tokens
/// do for [`MagicLinkIdentityProvider`]: the state that travels through
/// the browser hashes to the primary key, the PKCE verifier and nonce it
/// maps to never leave the server, and the row is claimed atomically on
/// callback so a state cannot be replayed.
pub struct OidcIdentityProvider {
    pool: sqlx::SqlitePool,
    /// Issuer identifier, trimmed of any trailing `/` — both for
    /// building the discovery URL and for validating the ID token.
    issuer_url: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    http: reqwest::Client,
    /// Discovery document, fetched once from the issuer and cached.
    discovery: tokio::sync::RwLock<Option<Discovery>>,
    /// How long a fresh login state stays valid.
    state_ttl: time::Duration,
}

impl OidcIdentityProvider {
    pub fn new(
        pool: sqlx::SqlitePool,
        config: &OidcConfig,
        base_url: &str,
        state_ttl: time::Duration,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        Ok(Self {
            pool,
            issuer_url: config.issuer_url.trim_end_matches('/').to_owned(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            redirect_uri: format!("{}/api/auth/oidc/callback", base_url.trim_end_matches('/')),
            http,
            discovery: tokio::sync::RwLock::new(None),
            state_ttl,
        })
    }

    /// Mint a fresh authorization request: the URL the browser is sent
    /// to, plus the `state` that must come back with it.
    pub async fn begin_login(
        &self,
        now: OffsetDateTime,
    ) -> Result<(String, String), IdentityError> {
        let discovery = self.discovery().await?;
        let state = hex(&(rand::rng().random::<[u8; 32]>()));
        // 32 random bytes, base64url: exactly 43 unreserved characters,
        // as PKCE requires of the verifier.
        let verifier = b64url(&rand::rng().random::<[u8; 32]>());
        let nonce = hex(&(rand::rng().random::<[u8; 32]>()));
        sqlx::query("INSERT INTO oidc_states (hash, verifier, nonce, expires_at, used) VALUES (?1, ?2, ?3, ?4, 0)")
            .bind(hash_token(&state))
            .bind(&verifier)
            .bind(&nonce)
            .bind(to_text(now + self.state_ttl).map_err(|e| IdentityError::Corrupt(e.to_string()))?)
            .execute(&self.pool)
            .await?;

        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        let params: Vec<(String, String)> = vec![
            ("response_type".into(), "code".into()),
            ("client_id".into(), self.client_id.clone()),
            ("redirect_uri".into(), self.redirect_uri.clone()),
            ("scope".into(), "openid email".into()),
            ("state".into(), state.clone()),
            ("nonce".into(), nonce),
            ("code_challenge".into(), challenge),
            ("code_challenge_method".into(), "S256".into()),
        ];
        let url = build_url(&discovery.authorization_endpoint, &params);
        Ok((url, state))
    }

    /// A login hint is only worth sending when it is a plausible email;
    /// callers pass raw user input and the IdP login screen is the
    /// better place for garbage.
    pub async fn start_login_with_hint(
        &self,
        email: &str,
        now: OffsetDateTime,
    ) -> Result<String, IdentityError> {
        let email = email.trim();
        if valid_email(email) {
            let (url, _state) = self.begin_login(now).await?;
            let url = format!("{url}&login_hint={}", url_encode(email));
            Ok(url)
        } else {
            self.begin_login(now).await.map(|(url, _)| url)
        }
    }

    /// Fetch (and cache) the issuer's discovery document.
    async fn discovery(&self) -> Result<Discovery, IdentityError> {
        if let Some(doc) = self.discovery.read().await.clone() {
            return Ok(doc);
        }
        let url = format!("{}/.well-known/openid-configuration", self.issuer_url);
        let doc: Discovery = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| IdentityError::Provider(format!("fetching discovery document: {e}")))?
            .json()
            .await
            .map_err(|e| IdentityError::Provider(format!("parsing discovery document: {e}")))?;

        // Mix-up defense: the document must describe the issuer we were
        // configured with, or we may be talking to someone else's IdP.
        if let Some(advertised) = &doc.issuer
            && advertised.trim_end_matches('/') != self.issuer_url
        {
            return Err(IdentityError::Provider(format!(
                "discovery document advertises issuer {advertised}, configured issuer is {}",
                self.issuer_url
            )));
        }
        *self.discovery.write().await = Some(doc.clone());
        Ok(doc)
    }

    /// Exchange the authorization code, validate the ID token, and
    /// return the organizer it proves.
    async fn verify_callback(
        &self,
        code: &str,
        state: &str,
        now: OffsetDateTime,
    ) -> Result<Organizer, IdentityError> {
        // Claim the state row first, atomically — a replay loses on the
        // `used = 0` guard, exactly like a magic token.
        let row =
            sqlx::query("SELECT verifier, nonce, expires_at FROM oidc_states WHERE hash = ?1")
                .bind(hash_token(state.trim()))
                .fetch_optional(&self.pool)
                .await?
                .ok_or(IdentityError::InvalidToken)?;
        let verifier: String = row
            .try_get("verifier")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        let nonce: String = row
            .try_get("nonce")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        let expires_at: String = row
            .try_get("expires_at")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        let expires_at = from_text(&expires_at, "expires_at")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        if expires_at <= now {
            return Err(IdentityError::InvalidToken);
        }
        let claimed = sqlx::query("UPDATE oidc_states SET used = 1 WHERE hash = ?1 AND used = 0")
            .bind(hash_token(state.trim()))
            .execute(&self.pool)
            .await?;
        if claimed.rows_affected() != 1 {
            return Err(IdentityError::InvalidToken);
        }

        let discovery = self.discovery().await?;
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code.trim()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("code_verifier", verifier.as_str()),
        ];
        let token_response = self
            .http
            .post(&discovery.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| IdentityError::Provider(format!("reaching token endpoint: {e}")))?;
        if !token_response.status().is_success() {
            // The IdP examined our request and said no: bad or used code.
            return Err(IdentityError::InvalidToken);
        }
        let token_body: serde_json::Value = token_response
            .json()
            .await
            .map_err(|e| IdentityError::Provider(format!("parsing token response: {e}")))?;
        let id_token = token_body
            .get("id_token")
            .and_then(|v| v.as_str())
            .ok_or(IdentityError::InvalidToken)?
            .to_owned();

        let claims = self
            .validate_id_token(&discovery.jwks_uri, &id_token)
            .await?;

        // The nonce ties this ID token to the authorization request we
        // minted; a token from any other flow must not authenticate.
        if claims.nonce.as_deref() != Some(nonce.as_str()) {
            return Err(IdentityError::InvalidToken);
        }
        // Strict by design: huddle has no other way to prove address
        // ownership, so the IdP must have.
        if claims.email_verified != Some(true) {
            return Err(IdentityError::InvalidToken);
        }

        let email = claims.email.trim();
        if !valid_email(email) {
            return Err(IdentityError::InvalidToken);
        }
        let id = ensure_organizer(&self.pool, email, now).await?;
        Ok(Organizer {
            id,
            email: email.to_owned(),
        })
    }

    /// Verify the ID token signature against the issuer's JWKS and its
    /// `iss`/`aud`/`exp` claims, and return the payload claims.
    async fn validate_id_token(
        &self,
        jwks_uri: &str,
        id_token: &str,
    ) -> Result<IdTokenClaims, IdentityError> {
        let jwks: serde_json::Value = self
            .http
            .get(jwks_uri)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| IdentityError::Provider(format!("fetching JWKS: {e}")))?
            .json()
            .await
            .map_err(|e| IdentityError::Provider(format!("parsing JWKS: {e}")))?;

        let keys = jwks
            .get("keys")
            .and_then(|k| k.as_array())
            .ok_or_else(|| IdentityError::Provider("JWKS has no keys array".into()))?;

        // Asymmetric algorithms only, and per key type: jsonwebtoken
        // requires every allowed algorithm to belong to the presented
        // key's family. Octet keys are skipped outright — accepting an
        // HMAC key here would let the IdP's public key double as an
        // HMAC secret (the classic algorithm-confusion attack).
        let mut last_err = IdentityError::InvalidToken;
        for key in keys {
            let Ok(jwk) = serde_json::from_value::<jsonwebtoken::jwk::Jwk>(key.clone()) else {
                continue;
            };
            let (family, algorithms) = match &jwk.algorithm {
                jsonwebtoken::jwk::AlgorithmParameters::EllipticCurve(_) => (
                    jsonwebtoken::Algorithm::ES256,
                    vec![
                        jsonwebtoken::Algorithm::ES256,
                        jsonwebtoken::Algorithm::ES384,
                    ],
                ),
                jsonwebtoken::jwk::AlgorithmParameters::RSA(_) => (
                    jsonwebtoken::Algorithm::RS256,
                    vec![
                        jsonwebtoken::Algorithm::RS256,
                        jsonwebtoken::Algorithm::RS384,
                        jsonwebtoken::Algorithm::RS512,
                        jsonwebtoken::Algorithm::PS256,
                        jsonwebtoken::Algorithm::PS384,
                        jsonwebtoken::Algorithm::PS512,
                    ],
                ),
                _ => continue,
            };
            let mut validation = jsonwebtoken::Validation::new(family);
            validation.algorithms = algorithms;
            validation.leeway = 60;
            validation.set_audience(&[&self.client_id]);
            let Ok(decoding_key) = jsonwebtoken::DecodingKey::from_jwk(&jwk) else {
                continue;
            };
            match jsonwebtoken::decode::<IdTokenClaims>(id_token, &decoding_key, &validation) {
                Ok(token) => {
                    // The signature verified against this key; validate
                    // the issuer ourselves, tolerating a trailing `/` on
                    // either side (issuers such as Auth0 emit
                    // `https://tenant/` as `iss` even when configured —
                    // and discovered — without the slash).
                    if token.claims.iss.trim_end_matches('/') != self.issuer_url {
                        return Err(IdentityError::Provider(format!(
                            "ID token issuer {} does not match {}",
                            token.claims.iss, self.issuer_url
                        )));
                    }
                    return Ok(token.claims);
                }
                Err(e) => last_err = IdentityError::Provider(format!("ID token rejected: {e}")),
            }
        }
        Err(last_err)
    }
}

impl IdentityProvider for OidcIdentityProvider {
    async fn start_login(&self, email: &str, now: OffsetDateTime) -> Result<String, IdentityError> {
        self.start_login_with_hint(email, now).await
    }

    async fn verify(
        &self,
        token: &str,
        state: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<Organizer, IdentityError> {
        // The state is what we minted; without it there is no row to
        // claim and no PKCE verifier to present.
        match state {
            Some(state) => self.verify_callback(token, state, now).await,
            None => Err(IdentityError::InvalidToken),
        }
    }
}

/// The configured identity flow, selected at startup. The trait is
/// async-fn-in-trait and therefore not dyn-safe, so this is a concrete
/// enum instead of a boxed trait object.
pub enum Identity {
    MagicLinks(MagicLinkIdentityProvider),
    /// Boxed: the OIDC provider carries an HTTP client and a discovery
    /// cache and dwarfs the magic-link variant.
    Oidc(Box<OidcIdentityProvider>),
}

impl Identity {
    /// True when SSO is configured and magic links are replaced.
    pub fn is_oidc(&self) -> bool {
        matches!(self, Identity::Oidc(_))
    }
}

impl IdentityProvider for Identity {
    async fn start_login(&self, email: &str, now: OffsetDateTime) -> Result<String, IdentityError> {
        match self {
            Identity::MagicLinks(p) => p.start_login(email, now).await,
            Identity::Oidc(p) => p.start_login(email, now).await,
        }
    }

    async fn verify(
        &self,
        token: &str,
        state: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<Organizer, IdentityError> {
        match self {
            Identity::MagicLinks(p) => p.verify(token, state, now).await,
            Identity::Oidc(p) => p.verify(token, state, now).await,
        }
    }
}

/// Append `params` to `base`, percent-encoding each value. Tolerates an
/// authorization endpoint that already carries a query string.
fn build_url(base: &str, params: &[(String, String)]) -> String {
    let mut url = base.to_owned();
    let separator = if base.contains('?') {
        if base.ends_with('?') || base.ends_with('&') {
            ""
        } else {
            "&"
        }
    } else {
        "?"
    };
    url.push_str(separator);
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{url}{query}")
}

/// Percent-encode for a URL query component (RFC 3986 unreserved set).
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(*byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    async fn provider() -> (MagicLinkIdentityProvider, sqlx::SqlitePool) {
        let pool = crate::db::test_pool().await;
        (
            MagicLinkIdentityProvider::new(pool.clone(), time::Duration::minutes(15)),
            pool,
        )
    }

    #[tokio::test]
    async fn magic_link_logins_create_the_organizer_on_first_use() {
        let (provider, _pool) = provider().await;
        let now = OffsetDateTime::now_utc();

        let token = provider
            .start_login("alice@example.com", now)
            .await
            .unwrap();
        let organizer = provider.verify(&token, None, now).await.unwrap();
        assert_eq!(organizer.email, "alice@example.com");

        // A second login mints a fresh token for the same organizer.
        let token = provider
            .start_login("alice@example.com", now)
            .await
            .unwrap();
        let again = provider.verify(&token, None, now).await.unwrap();
        assert_eq!(again.id, organizer.id);
    }

    #[tokio::test]
    async fn magic_links_are_single_use() {
        let (provider, _pool) = provider().await;
        let now = OffsetDateTime::now_utc();
        let token = provider.start_login("bob@example.com", now).await.unwrap();

        provider.verify(&token, None, now).await.unwrap();
        assert!(matches!(
            provider.verify(&token, None, now).await,
            Err(IdentityError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn magic_links_expire() {
        let (provider, _pool) = provider().await;
        let now = OffsetDateTime::now_utc();
        let token = provider
            .start_login("carol@example.com", now)
            .await
            .unwrap();

        let later = now + time::Duration::minutes(16);
        assert!(matches!(
            provider.verify(&token, None, later).await,
            Err(IdentityError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn unknown_and_malformed_tokens_are_rejected() {
        let (provider, _pool) = provider().await;
        let now = OffsetDateTime::now_utc();
        assert!(matches!(
            provider.verify("not-a-token", None, now).await,
            Err(IdentityError::InvalidToken)
        ));
        assert!(matches!(
            provider.start_login("no-at-sign", now).await,
            Err(IdentityError::InvalidEmail)
        ));
        assert!(matches!(
            provider.start_login("double@@at", now).await,
            Err(IdentityError::InvalidEmail)
        ));
    }

    #[tokio::test]
    async fn only_the_hash_is_stored() {
        let (provider, pool) = provider().await;
        let now = OffsetDateTime::now_utc();
        let token = provider.start_login("dave@example.com", now).await.unwrap();

        // The token itself must never appear in the database.
        let row: String = sqlx::query("SELECT hash FROM login_tokens")
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get("hash")
            .unwrap();
        assert_eq!(row, hash_token(&token));
        assert_ne!(row, token);
    }

    // --- OIDC provider ---------------------------------------------------

    use crate::config::OidcConfig;
    use crate::oidc_test_idp::FakeIdp;
    use std::collections::HashMap;

    /// Percent-decode a query value (huddle only ever encodes the
    /// `%XX` form).
    fn percent_decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%'
                && i + 2 < bytes.len()
                && bytes[i + 1].is_ascii_hexdigit()
                && bytes[i + 2].is_ascii_hexdigit()
            {
                let hex_pair = |b: u8| -> u8 {
                    match b {
                        b'0'..=b'9' => b - b'0',
                        b'a'..=b'f' => b - b'a' + 10,
                        b'A'..=b'F' => b - b'A' + 10,
                        _ => 0,
                    }
                };
                out.push(hex_pair(bytes[i + 1]) * 16 + hex_pair(bytes[i + 2]));
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The query params of a URL, percent-decoded.
    fn query_params(url: &str) -> HashMap<String, String> {
        let query = url.split('?').nth(1).unwrap_or_default();
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.to_owned(), percent_decode(v)))
            .collect()
    }

    async fn oidc_provider(
        ttl: time::Duration,
    ) -> (OidcIdentityProvider, FakeIdp, sqlx::SqlitePool, OidcConfig) {
        let pool = crate::db::test_pool().await;
        let idp = FakeIdp::start("test-client", "test-secret", "sso@example.com")
            .await
            .unwrap();
        let config = OidcConfig {
            issuer_url: idp.base_url.clone(),
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
        };
        let provider =
            OidcIdentityProvider::new(pool.clone(), &config, "http://127.0.0.1:3134", ttl).unwrap();
        (provider, idp, pool, config)
    }

    /// Drive one full SSO login; returns the organizer and the parsed
    /// authorization URL.
    async fn sso_login(
        provider: &OidcIdentityProvider,
        idp: &FakeIdp,
        now: OffsetDateTime,
    ) -> (Organizer, HashMap<String, String>, String) {
        let (url, state) = provider.begin_login(now).await.unwrap();
        let params = query_params(&url);
        idp.set_nonce(&params["nonce"]);
        let organizer = provider
            .verify("some-auth-code", Some(&state), now)
            .await
            .unwrap();
        (organizer, params, state)
    }

    #[tokio::test]
    async fn oidc_login_urls_carry_pkce_state_and_nonce() {
        let (provider, _idp, _pool, config) = oidc_provider(time::Duration::minutes(15)).await;
        let (url, state) = provider
            .begin_login(OffsetDateTime::now_utc())
            .await
            .unwrap();

        let base = url.split('?').next().unwrap();
        assert!(
            base.contains("/authorize"),
            "must hit the discovered authorization endpoint: {url}"
        );
        let params = query_params(&url);
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["client_id"], config.client_id);
        assert_eq!(
            params["redirect_uri"],
            "http://127.0.0.1:3134/api/auth/oidc/callback"
        );
        assert_eq!(params["scope"], "openid email");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["state"], state);
        assert_eq!(state.len(), 64, "state is 32 hex bytes");
        assert!(
            state.chars().all(|c| c.is_ascii_hexdigit()),
            "state must be hex"
        );
        assert_eq!(params["nonce"].len(), 64, "nonce is 32 hex bytes");
        assert_eq!(
            params["code_challenge"].len(),
            43,
            "S256 challenge is 43 b64url chars"
        );

        // A plausible email becomes a login hint; garbage does not.
        let hinted = provider
            .start_login_with_hint("sso@example.com", OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert!(hinted.contains("login_hint="), "hinted: {hinted}");
        let unhinted = provider
            .start_login_with_hint("not-an-email", OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert!(!unhinted.contains("login_hint="), "unhinted: {unhinted}");
    }

    #[tokio::test]
    async fn oidc_verify_happy_path_checks_pkce_and_logs_in() {
        let (provider, idp, _pool, _config) = oidc_provider(time::Duration::minutes(15)).await;
        let now = OffsetDateTime::now_utc();
        let (organizer, params, state) = sso_login(&provider, &idp, now).await;

        assert_eq!(organizer.email, "sso@example.com");

        // The PKCE challenge in the URL is the S256 of the verifier the
        // callback presented to the IdP.
        let verifier = idp.verifier_seen().expect("callback presented a verifier");
        assert_eq!(
            params["code_challenge"],
            b64url(&Sha256::digest(verifier.as_bytes()))
        );

        // Single-use: replaying the state must fail even with a fresh code.
        assert!(matches!(
            provider.verify("another-code", Some(&state), now).await,
            Err(IdentityError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn oidc_states_expire() {
        let (provider, idp, _pool, _config) = oidc_provider(time::Duration::minutes(1)).await;
        let now = OffsetDateTime::now_utc();
        let (url, state) = provider.begin_login(now).await.unwrap();
        idp.set_nonce(&query_params(&url)["nonce"]);

        let later = now + time::Duration::minutes(2);
        assert!(matches!(
            provider.verify("code", Some(&state), later).await,
            Err(IdentityError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn oidc_rejects_wrong_nonce_and_missing_state() {
        let (provider, idp, _pool, _config) = oidc_provider(time::Duration::minutes(15)).await;
        let now = OffsetDateTime::now_utc();

        // The IdP minted a token for someone else's authorization
        // request: the nonce must not match ours.
        let (_url, state) = provider.begin_login(now).await.unwrap();
        idp.set_nonce("not-our-nonce");
        assert!(matches!(
            provider.verify("code", Some(&state), now).await,
            Err(IdentityError::InvalidToken)
        ));

        // And a callback without the state binding cannot even claim a row.
        let (url, _state) = provider.begin_login(now).await.unwrap();
        idp.set_nonce(&query_params(&url)["nonce"]);
        assert!(matches!(
            provider.verify("code", None, now).await,
            Err(IdentityError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn oidc_and_magic_link_logins_are_interchangeable() {
        let (provider, idp, pool, _config) = oidc_provider(time::Duration::minutes(15)).await;
        let now = OffsetDateTime::now_utc();
        let (sso, _params, _state) = sso_login(&provider, &idp, now).await;

        // The same person signs in with a magic link: same organizer.
        let magic = MagicLinkIdentityProvider::new(pool, time::Duration::minutes(15));
        let token = magic.start_login("sso@example.com", now).await.unwrap();
        let via_link = magic.verify(&token, None, now).await.unwrap();
        assert_eq!(via_link.id, sso.id);
        assert_eq!(via_link.email, sso.email);
    }

    #[tokio::test]
    async fn oidc_issuer_mismatch_is_rejected() {
        // The discovery document must describe the issuer we were
        // configured with, or we may be talking to someone else's IdP.
        let (provider, idp, _pool, _config) = oidc_provider(time::Duration::minutes(15)).await;
        idp.set_issuer("http://127.0.0.1:9/someone-else");
        assert!(matches!(
            provider.begin_login(OffsetDateTime::now_utc()).await,
            Err(IdentityError::Provider(_))
        ));
    }

    #[tokio::test]
    async fn oidc_issuer_trailing_slash_is_tolerated() {
        // Issuers such as Auth0 emit `iss` with a trailing slash even
        // when the server was configured — and the IdP discovered —
        // without one; the claim check must be normalized, not exact.
        let (provider, idp, _pool, config) = oidc_provider(time::Duration::minutes(15)).await;
        idp.set_issuer(&format!("{}/", config.issuer_url));
        let now = OffsetDateTime::now_utc();
        let (organizer, _params, _state) = sso_login(&provider, &idp, now).await;
        assert_eq!(organizer.email, "sso@example.com");
    }
}
