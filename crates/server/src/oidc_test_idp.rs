//! A minimal in-process OIDC issuer for tests.
//!
//! Implements exactly the endpoints [`crate::identity::OidcIdentityProvider`]
//! talks to: discovery, JWKS, and the token exchange. ID tokens are
//! signed ES256 with a static throwaway keypair — nothing here is a
//! secret; it exists so the real code path (PKCE, JWKS validation,
//! nonce, single-use state) is exercised against something that behaves
//! like an IdP.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;
use time::OffsetDateTime;

/// Throwaway P-256 key, PKCS#8 PEM (jsonwebtoken 9 only accepts PKCS#8
/// EC keys). Test-only; never a secret.
const PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgLjyO+DN+81HLrAoY\nXS4wmv6bPGtGskZ5DmLDM5KBsfyhRANCAAQEVpvSAfNlrfFS6YmYAQ9qAwMxkD0u\nXIxbNCV2egBxxtK1XZGjAXTeOrCbTIEXcs9aecohpuqkZ1MFGPjHocep\n-----END PRIVATE KEY-----";

/// The matching public key, as a JWK (built at runtime; `json!` is not
/// const-evaluable).
fn public_jwk() -> serde_json::Value {
    json!({
        "kty": "EC",
        "x": "BFab0gHzZa3xUumJmAEPagMDMZA9LlyMWzQldnoAccY",
        "y": "0rVdkaMBdN46sJtMgRdyz1p5yiGm6qRnUwUY-Mehx6k",
        "crv": "P-256",
        "alg": "ES256",
        "use": "sig",
    })
}

/// Controls the tests hold on the running issuer.
#[derive(Clone, Default)]
struct Inner {
    /// The nonce minted into the next ID token (set by the test to the
    /// nonce from the authorization URL).
    nonce: Option<String>,
    /// The PKCE verifier presented at the token exchange.
    verifier_seen: Option<String>,
    /// When set, discovery/ID tokens advertise this issuer instead of
    /// the fake's own URL (mix-up-defense tests).
    issuer_override: Option<String>,
}

#[derive(Clone)]
struct Shared {
    base_url: String,
    client_id: String,
    client_secret: String,
    email: String,
    inner: Arc<std::sync::Mutex<Inner>>,
}

/// A running fake issuer.
#[derive(Clone)]
pub struct FakeIdp {
    pub base_url: String,
    shared: Shared,
}

impl FakeIdp {
    /// Start the fake issuer on an ephemeral loopback port, minting
    /// tokens for `email` as the given OAuth2 client.
    pub async fn start(client_id: &str, client_secret: &str, email: &str) -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        let shared = Shared {
            base_url: base_url.clone(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            email: email.into(),
            inner: Arc::new(std::sync::Mutex::new(Inner::default())),
        };

        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .with_state(shared.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("fake idp serves");
        });

        Ok(Self { base_url, shared })
    }

    /// The nonce to mint into the next ID token — the test echoes the
    /// nonce from the authorization URL here.
    pub fn set_nonce(&self, nonce: &str) {
        self.shared.inner.lock().unwrap().nonce = Some(nonce.to_owned());
    }

    /// The PKCE verifier the callback presented, once it has happened.
    pub fn verifier_seen(&self) -> Option<String> {
        self.shared.inner.lock().unwrap().verifier_seen.clone()
    }

    /// Advertise a different issuer in discovery and ID tokens.
    pub fn set_issuer(&self, issuer: &str) {
        self.shared.inner.lock().unwrap().issuer_override = Some(issuer.to_owned());
    }
}

async fn discovery(State(state): State<Shared>) -> Json<serde_json::Value> {
    let base = &state.base_url;
    // Normally the fake advertises itself; tests may override it to
    // exercise the client's mix-up defense.
    let issuer = state
        .inner
        .lock()
        .unwrap()
        .issuer_override
        .clone()
        .unwrap_or_else(|| base.clone());
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "jwks_uri": format!("{base}/jwks"),
    }))
}

async fn jwks() -> Json<serde_json::Value> {
    Json(json!({ "keys": [public_jwk()] }))
}

async fn token(State(state): State<Shared>, Form(form): Form<HashMap<String, String>>) -> Response {
    if form.get("grant_type").map(String::as_str) != Some("authorization_code")
        || form.get("code").is_none_or(|c| c.is_empty())
    {
        return error(StatusCode::BAD_REQUEST, "unsupported_grant");
    }
    if form.get("client_id").map(String::as_str) != Some(state.client_id.as_str())
        || form.get("client_secret").map(String::as_str) != Some(state.client_secret.as_str())
    {
        return error(StatusCode::UNAUTHORIZED, "invalid_client");
    }

    let nonce = state.inner.lock().unwrap().nonce.clone();
    if let Some(verifier) = form.get("code_verifier") {
        state.inner.lock().unwrap().verifier_seen = Some(verifier.clone());
    }

    let now = OffsetDateTime::now_utc().unix_timestamp();
    let issuer = state
        .inner
        .lock()
        .unwrap()
        .issuer_override
        .clone()
        .unwrap_or_else(|| state.base_url.clone());
    let claims = json!({
        "iss": issuer,
        "sub": format!("user-{}", state.email),
        "aud": state.client_id,
        "exp": now + 3600,
        "iat": now,
        "email": state.email,
        "email_verified": true,
        "nonce": nonce,
    });
    let key = jsonwebtoken::EncodingKey::from_ec_pem(PRIVATE_KEY_PEM.as_bytes())
        .expect("static test key parses");
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    let id_token = jsonwebtoken::encode(&header, &claims, &key).expect("sign id token");
    Json(json!({
        "access_token": "fake-access-token",
        "token_type": "Bearer",
        "id_token": id_token,
    }))
    .into_response()
}

fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "error": code }))).into_response()
}
