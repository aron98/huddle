//! TLS termination (issue #30, ADR 008): the binary itself serves
//! HTTPS/WSS — from PEM files on disk (static) or with automatic
//! Let's Encrypt certificates (ACME, TLS-ALPN-01) — instead of
//! delegating to a reverse proxy. Both paths run on `axum-server`
//! with rustls, keep `ConnectInfo` (per-IP rate limiting needs the
//! socket address), and shut down gracefully.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use tracing::{info, warn};

use crate::config::{Config, TlsSelection};

/// The fully wired app: routes plus `ConnectInfo`, ready to serve.
type MakeService =
    axum::extract::connect_info::IntoMakeServiceWithConnectInfo<axum::Router, SocketAddr>;

/// Pin rustls's process-level crypto provider to aws-lc-rs.
///
/// The dependency graph enables two rustls providers (`ring` via
/// lettre/reqwest, `aws-lc-rs` via axum-server/rustls-acme), so rustls
/// cannot auto-detect one from crate features and would panic at the
/// first handshake. Installing explicitly picks the provider the rest
/// of the stack already builds with; the `Err` case (a provider was
/// installed earlier in this process) is fine to ignore.
fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Serve `app` with TLS per the validated `selection` (ADR 008).
///
/// Protocol details are left at rustls's safe defaults (TLS 1.2/1.3,
/// modern cipher suites) — deliberately not hand-rolled; see the ADR.
pub async fn serve(
    config: &Config,
    selection: TlsSelection,
    app: axum::Router,
) -> anyhow::Result<()> {
    install_crypto_provider();
    match selection {
        TlsSelection::Static { cert, key } => {
            serve_static(
                config,
                &cert,
                &key,
                app.into_make_service_with_connect_info(),
            )
            .await
        }
        TlsSelection::Acme {
            domains,
            contact,
            staging,
        } => {
            serve_acme(
                config,
                domains,
                contact,
                staging,
                app.into_make_service_with_connect_info(),
            )
            .await
        }
    }
}

/// Terminate TLS with a certificate chain and key from PEM files.
async fn serve_static(
    config: &Config,
    cert: &Path,
    key: &Path,
    make: MakeService,
) -> anyhow::Result<()> {
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
        .await
        .with_context(|| {
            format!(
                "loading TLS certificate {} and key {}",
                cert.display(),
                key.display()
            )
        })?;
    let handle = axum_server::Handle::new();
    spawn_graceful_shutdown(handle.clone());
    info!(bind = %config.bind, "serving HTTPS (static certificate)");
    axum_server::bind_rustls(config.bind, rustls_config)
        .handle(handle)
        .serve(make)
        .await
        .context("TLS server error")?;
    Ok(())
}

/// Terminate TLS with certificates acquired from Let's Encrypt over
/// TLS-ALPN-01: challenges and regular traffic share the same port,
/// so no extra listener is needed. Account and certificate state is
/// cached under `data_dir/acme-cache` to stay inside Let's Encrypt's
/// rate limits across restarts.
async fn serve_acme(
    config: &Config,
    domains: Vec<String>,
    contact: Option<String>,
    staging: bool,
    make: MakeService,
) -> anyhow::Result<()> {
    let cache_dir = config.data_dir.join("acme-cache");
    let mut acme = rustls_acme::AcmeConfig::new(domains.clone());
    if let Some(contact) = contact.as_deref() {
        acme = acme.contact_push(contact_url(contact));
    }
    let mut state = acme
        .cache(rustls_acme::caches::DirCache::new(cache_dir.clone()))
        .directory_lets_encrypt(!staging)
        .state();
    // The acceptor hands out the acquired certificates; the state
    // itself is a stream that must be polled to drive ordering and
    // renewal — no persistent task is spawned under the hood, so we
    // poll it ourselves and log the lifecycle.
    // rustls-acme's default config leaves ALPN unset; advertise h2 +
    // HTTP/1.1 like the static path does so browsers can use HTTP/2.
    let mut rustls_config = (*state.default_rustls_config()).clone();
    rustls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = state.axum_acceptor(std::sync::Arc::new(rustls_config));
    tokio::spawn(async move {
        use futures::StreamExt;
        while let Some(event) = state.next().await {
            match event {
                Ok(event) => info!("acme: {event:?}"),
                Err(err) => warn!("acme error: {err:?}"),
            }
        }
    });

    let handle = axum_server::Handle::new();
    spawn_graceful_shutdown(handle.clone());
    info!(
        bind = %config.bind,
        cache = %cache_dir.display(),
        staging,
        "serving HTTPS (automatic Let's Encrypt certificates)"
    );
    axum_server::bind(config.bind)
        .acceptor(acceptor)
        .handle(handle)
        .serve(make)
        .await
        .context("TLS server error")?;
    Ok(())
}

/// Let's Encrypt wants ACME account contacts as URLs, config files want
/// to write bare emails.
fn contact_url(contact: &str) -> String {
    if contact.starts_with("mailto:") {
        contact.to_owned()
    } else {
        format!("mailto:{contact}")
    }
}

/// Wire SIGINT to a graceful drain, mirroring the plain-HTTP path's
/// `with_graceful_shutdown`: in-flight requests get ten seconds to
/// finish before connections are cut.
fn spawn_graceful_shutdown(handle: axum_server::Handle<SocketAddr>) {
    tokio::spawn(async move {
        crate::shutdown_signal().await;
        handle.graceful_shutdown(Some(Duration::from_secs(10)));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real rustls handshake against a self-signed certificate: serve
    /// the app over TLS on an ephemeral port, then fetch `/api/health`
    /// with a client that accepts the self-signed chain and assert the
    /// response (including the TLS-only HSTS header).
    #[tokio::test]
    async fn static_tls_handshake_serves_https() -> anyhow::Result<()> {
        install_crypto_provider();
        let dir = std::env::temp_dir().join(format!(
            "huddle-tls-handshake-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir)?;
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
        std::fs::write(&cert_path, cert.cert.pem())?;
        std::fs::write(&key_path, cert.key_pair.serialize_pem())?;

        let state = crate::http::test_state().await;
        let app = crate::http::with_tls_headers(crate::http::router(state), true);

        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path).await?;
        let handle = axum_server::Handle::new();
        let make = app.into_make_service_with_connect_info::<SocketAddr>();
        let server = axum_server::from_tcp_rustls(listener, rustls_config)?;
        tokio::spawn(async move {
            let _ = server.handle(handle).serve(make).await;
        });

        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()?;
        let response = client
            .get(format!("https://{addr}/api/health"))
            .send()
            .await?;
        assert!(response.status().is_success());
        assert_eq!(
            response
                .headers()
                .get("strict-transport-security")
                .and_then(|v| v.to_str().ok()),
            Some("max-age=31536000; includeSubDomains")
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    /// Issue #65: the whole point of built-in TLS is that `/ws` rides
    /// the same HTTPS listener, but nothing proved it end-to-end. Open
    /// a real `wss://` signaling connection against the self-signed
    /// server (trusting only its own leaf as root) and run a join
    /// round-trip: `join` in, `Welcome` back with the protocol version.
    #[tokio::test]
    async fn static_tls_carries_wss_signaling() -> anyhow::Result<()> {
        use futures::{SinkExt, StreamExt};
        use huddle_protocol::{CreateMeetingRequest, PROTOCOL_VERSION};
        use time::OffsetDateTime;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        install_crypto_provider();
        let dir = std::env::temp_dir().join(format!(
            "huddle-tls-wss-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir)?;
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        let cert =
            rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
        std::fs::write(&cert_path, cert.cert.pem())?;
        std::fs::write(&key_path, cert.key_pair.serialize_pem())?;

        let state = crate::http::test_state().await;
        let app = crate::http::with_tls_headers(crate::http::router(state.clone()), true);

        // A meeting to join — created straight in the database, the
        // same way the session tests do it.
        let now = OffsetDateTime::now_utc();
        let req = CreateMeetingRequest {
            title: "TLS signaling test".into(),
            starts_at: now,
            ends_at: now + time::Duration::hours(1),
            repeat: None,
            passcode: None,
            e2ee: false,
        };
        let code = crate::meetings::create(state.pool(), &req, None, now)
            .await?
            .code;

        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert_path, &key_path).await?;
        let handle = axum_server::Handle::new();
        let make = app.into_make_service_with_connect_info::<SocketAddr>();
        let server = axum_server::from_tcp_rustls(listener, rustls_config)?;
        tokio::spawn(async move {
            let _ = server.handle(handle).serve(make).await;
        });

        // The client trusts exactly the self-signed leaf — no
        // danger-accept verifier, a normal root store with our cert.
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone())?;
        let client_config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();

        let request = format!("wss://{addr}/ws").into_client_request()?;
        let (mut ws, _) = tokio_tungstenite::connect_async_tls_with_config(
            request,
            None,
            false,
            Some(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
                client_config,
            ))),
        )
        .await?;

        let join = serde_json::json!({
            "type": "join",
            "meeting_code": code,
            "name": "tls-client",
        });
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            join.to_string().into(),
        ))
        .await?;

        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("socket closed before Welcome"))??;
        let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
            anyhow::bail!("expected text frame, got {frame:?}");
        };
        let huddle_protocol::ServerMessage::Welcome {
            protocol_version, ..
        } = serde_json::from_str(&text)?
        else {
            anyhow::bail!("expected Welcome, got {text}");
        };
        assert_eq!(protocol_version, PROTOCOL_VERSION);

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
