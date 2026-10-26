//! How the organizer signs in on this server — probed before the
//! login form is ever used (issue #99).
//!
//! Pure decision logic, no browser glue, so the native test build
//! covers it: the wasm-only `Login` component fetches
//! `GET /api/auth/oidc/status` on mount and asks
//! [`sso_entry_from_probe`] what to do with the answer.

/// The same-origin SSO entry point the login page jumps to when the
/// server has an OIDC IdP configured — the same route the magic-link
/// refusal body points at (issue #89).
pub const OIDC_LOGIN_ENTRY: &str = "/api/auth/oidc/login";

/// What a `GET /api/auth/oidc/status` probe told the login page.
///
/// `Some(OIDC_LOGIN_ENTRY)` when the server advertises an OIDC IdP:
/// the login page sends the browser straight there, instead of
/// rendering a magic-link form whose submission would only get
/// refused. `None` keeps the magic-link form completely unchanged —
/// no IdP configured, an older server without the status route, a
/// non-200, or an unparseable body: a failed probe must degrade to
/// the form, never a blank page.
pub fn sso_entry_from_probe(status: u16, body: &str) -> Option<&'static str> {
    if status != 200 {
        return None;
    }
    let oidc_configured = serde_json::from_str::<serde_json::Value>(body)
        .map(|value| {
            value
                .get("status")
                .and_then(|status| status.as_str())
                .is_some_and(|status| status == "oidc_configured")
        })
        .unwrap_or(false);
    oidc_configured.then_some(OIDC_LOGIN_ENTRY)
}

#[cfg(test)]
mod tests {
    use super::{OIDC_LOGIN_ENTRY, sso_entry_from_probe};

    #[test]
    fn a_configured_probe_yields_the_sso_entry() {
        // The exact body the real server answers with: the issuer is
        // public (issue #88), the client secret never crosses the wire.
        let body = r#"{"status":"oidc_configured","issuer":"https://idp.example.com"}"#;
        assert_eq!(sso_entry_from_probe(200, body), Some(OIDC_LOGIN_ENTRY));
    }

    #[test]
    fn a_magic_link_server_keeps_the_form() {
        assert_eq!(
            sso_entry_from_probe(200, r#"{"status":"oidc_not_configured"}"#),
            None
        );
    }

    #[test]
    fn probe_failures_fall_back_to_the_form() {
        // An older server without the status route: the fallback
        // serves the JSON 404 envelope, never a 200.
        assert_eq!(sso_entry_from_probe(404, r#"{"error":{"code":"x"}}"#), None);
        // Even a configured-looking body behind a non-200 is refused.
        assert_eq!(
            sso_entry_from_probe(500, r#"{"status":"oidc_configured"}"#),
            None
        );
        // Unparseable or empty bodies never claim an IdP.
        assert_eq!(sso_entry_from_probe(200, "not json"), None);
        assert_eq!(sso_entry_from_probe(200, ""), None);
    }
}
