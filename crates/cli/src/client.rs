//! One thin blocking REST client over the meeting and OAuth endpoints,
//! with transparent access-token refresh: any command (and every MCP
//! tool call) goes through here, so "refresh is invisible" has a
//! single implementation.
//!
//! Error taxonomy maps straight onto process exit codes (issue #9):
//! [`CliError::Usage`] → 1, [`CliError::Auth`] → 2,
//! [`CliError::Server`] → 3.

use std::{path::Path, time::Duration};

use huddle_protocol::{
    ApiError, CreateMeetingRequest, MeetingResponse, OAuthError, OAuthTokenResponse,
};
use reqwest::{
    Method, StatusCode,
    blocking::Client as HttpClient,
    header::{CONTENT_TYPE, HeaderValue},
};
use time::OffsetDateTime;

use crate::store::{self, StoredLogin};

/// The client name shown on the approval page and the dashboard.
pub const CLIENT_NAME: &str = "huddle-cli";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Refresh when the cached access token would die within this window.
const EXPIRY_MARGIN_SEC: i64 = 5;

/// How the CLI fails, and the process exit code each kind maps to.
#[derive(Debug, PartialEq)]
pub enum CliError {
    /// Exit 1: bad invocation — malformed values, missing flags.
    Usage(String),
    /// Exit 2: no stored login, or it was rejected, expired, or revoked.
    Auth(String),
    /// Exit 3: the server refused the request (or could not be reached).
    Server(String),
}

/// The human-facing shape of one CLI error (used by both the command
/// layer and the MCP tool wrapper).
pub fn describe(error: &CliError) -> String {
    match error {
        CliError::Usage(message) => message.clone(),
        CliError::Auth(message) => format!("auth: {message}"),
        CliError::Server(message) => format!("server: {message}"),
    }
}

/// Unix seconds now.
pub fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

pub struct Client {
    http: HttpClient,
    server: String,
    token_file: std::path::PathBuf,
    login: Option<StoredLogin>,
}

impl Client {
    /// Build a client. The server URL comes from the flag, or the one
    /// recorded at login; the login is loaded fresh so a long-running
    /// MCP server sees logins made after it started.
    pub fn new(server: Option<String>, token_file: Option<&Path>) -> Client {
        let path = store::token_file(token_file);
        let stored = store::load_from(&path);
        let server = server
            .or_else(|| stored.as_ref().map(|s| s.server.clone()))
            .unwrap_or_default();
        let http = HttpClient::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_default();
        Client {
            http,
            server,
            token_file: path,
            login: stored,
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn login(&self) -> Option<&StoredLogin> {
        self.login.as_ref()
    }

    /// The server base URL, or a usage error explaining how to get one.
    pub fn require_server(&self) -> Result<&str, CliError> {
        if self.server.trim().is_empty() {
            return Err(CliError::Usage(
                "no server known; pass --server <url> (or run `huddle-cli login` first)".to_owned(),
            ));
        }
        Ok(self.server.trim_end_matches('/'))
    }

    /// The browser join URL for a meeting code.
    pub fn join_url(&self, code: &str) -> Result<String, CliError> {
        Ok(format!("{}/m/{code}", self.require_server()?))
    }

    pub fn set_login(&mut self, login: StoredLogin) {
        self.server = login.server.clone();
        self.login = Some(login);
    }

    pub fn save_login(&self) -> Result<(), CliError> {
        if let Some(login) = &self.login {
            store::save_to(&self.token_file, login)
                .map_err(|e| CliError::Server(format!("could not save the login: {e}")))?;
        }
        Ok(())
    }

    /// Wipe the stored login locally (logout, or a rejected refresh).
    pub fn forget_login(&mut self) {
        store::clear_at(&self.token_file);
        self.login = None;
    }

    /// A valid bearer access token, refreshing and persisting if the
    /// cached one is missing or about to expire.
    pub fn ensure_access(&mut self) -> Result<String, CliError> {
        if let Some(login) = self.login.as_ref()
            && let (Some(token), Some(expires)) = (&login.access_token, login.access_expires_at)
            && expires - EXPIRY_MARGIN_SEC > now_unix()
        {
            return Ok(token.clone());
        }
        self.refresh_access()
    }

    /// Exchange the refresh token for a fresh access token. Only a
    /// definitive HTTP 400/401 kills the stored login (revoked or
    /// expired); transport errors and rate limits leave it alone.
    fn refresh_access(&mut self) -> Result<String, CliError> {
        let Some(login) = self.login.clone() else {
            return Err(CliError::Auth(
                "not logged in; run `huddle-cli login --server <url>` first".to_owned(),
            ));
        };
        let server = self.require_server()?.to_owned();
        let response = self
            .http
            .post(format!("{server}/api/oauth/token"))
            .form(&[
                ("grant_type", "refresh_token".to_owned()),
                ("refresh_token", login.refresh_token.clone()),
            ])
            .send()
            .map_err(|e| CliError::Server(format!("could not reach {server}: {e}")))?;
        let status = response.status();
        let body = response
            .text()
            .map_err(|e| CliError::Server(format!("unreadable token response: {e}")))?;
        if status.is_success() {
            let token: OAuthTokenResponse = serde_json::from_str(&body)
                .map_err(|e| CliError::Server(format!("unreadable token response: {e}")))?;
            let access = token.access_token.clone();
            self.set_login(StoredLogin {
                server: login.server,
                refresh_token: login.refresh_token,
                access_token: Some(token.access_token),
                access_expires_at: Some(now_unix() + token.expires_in as i64),
            });
            self.save_login()?;
            return Ok(access);
        }
        if status == StatusCode::BAD_REQUEST || status == StatusCode::UNAUTHORIZED {
            self.forget_login();
            let detail = serde_json::from_str::<OAuthError>(&body)
                .map(|error| error.error)
                .unwrap_or_else(|_| format!("HTTP {status}"));
            return Err(CliError::Auth(format!(
                "the stored login was rejected ({detail}); run `huddle-cli login` again"
            )));
        }
        Err(CliError::Server(format!(
            "token endpoint answered HTTP {status}: {body}"
        )))
    }

    /// Authenticated JSON request. Transparently refreshes, and retries
    /// exactly once when the server says 401 (a restarted server may
    /// have re-keyed while our access token was still nominally valid).
    pub fn send_json(
        &mut self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<(u16, String), CliError> {
        let token = self.ensure_access()?;
        let (status, text) = self.try_send(&token, method.clone(), path, body.clone())?;
        if status != 401 {
            return Ok((status, text));
        }
        if let Some(login) = self.login.as_mut() {
            login.access_token = None;
            login.access_expires_at = None;
        }
        let token = self.refresh_access()?;
        self.try_send(&token, method, path, body)
    }

    fn try_send(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> Result<(u16, String), CliError> {
        let server = self.require_server()?.to_owned();
        let mut request = self
            .http
            .request(method, format!("{server}{path}"))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .body(body);
        }
        let response = request
            .send()
            .map_err(|e| CliError::Server(format!("could not reach {server}: {e}")))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .map_err(|e| CliError::Server(format!("unreadable response body: {e}")))?;
        Ok((status, text))
    }

    /// Unauthenticated form POST (the OAuth endpoints themselves).
    pub fn post_form(
        &self,
        path: &str,
        form: &[(&str, String)],
    ) -> Result<(u16, String), CliError> {
        let server = self.require_server()?.to_owned();
        let response = self
            .http
            .post(format!("{server}{path}"))
            .form(form)
            .send()
            .map_err(|e| CliError::Server(format!("could not reach {server}: {e}")))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .map_err(|e| CliError::Server(format!("unreadable response body: {e}")))?;
        Ok((status, text))
    }

    /// Unauthenticated GET (public endpoints, e.g. the ICS export).
    pub fn get_public(&self, path: &str) -> Result<(u16, String), CliError> {
        let server = self.require_server()?.to_owned();
        let response = self
            .http
            .get(format!("{server}{path}"))
            .send()
            .map_err(|e| CliError::Server(format!("could not reach {server}: {e}")))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .map_err(|e| CliError::Server(format!("unreadable response body: {e}")))?;
        Ok((status, text))
    }

    /// POST /api/meetings — create and return the shaped response.
    pub fn create_meeting(
        &mut self,
        title: &str,
        starts_at: OffsetDateTime,
        ends_at: OffsetDateTime,
    ) -> Result<MeetingResponse, CliError> {
        let request = CreateMeetingRequest {
            title: title.to_owned(),
            starts_at,
            ends_at,
            repeat: None,
            passcode: None,
            e2ee: false,
        };
        // Mirror the server's validation client-side so a bad title is
        // a usage error, not a network round-trip.
        if let Err(message) = request.validate() {
            return Err(CliError::Usage(message.to_owned()));
        }
        let body = serde_json::to_string(&request)
            .map_err(|e| CliError::Server(format!("could not encode the meeting: {e}")))?;
        let (status, text) = self.send_json(Method::POST, "/api/meetings", Some(body))?;
        if (200..300).contains(&status) {
            return serde_json::from_str(&text)
                .map_err(|e| CliError::Server(format!("unreadable meeting response: {e}")));
        }
        Err(status_error(status, &text))
    }

    /// GET /api/meetings/mine (needs `meetings:read`).
    pub fn list_meetings(&mut self) -> Result<Vec<MeetingResponse>, CliError> {
        let (status, text) = self.send_json(Method::GET, "/api/meetings/mine", None)?;
        if status != 200 {
            return Err(status_error(status, &text));
        }
        serde_json::from_str(&text)
            .map_err(|e| CliError::Server(format!("unreadable meeting list: {e}")))
    }

    /// GET /api/meetings/{code} (public; auth keeps one error path).
    pub fn get_meeting(&mut self, code: &str) -> Result<MeetingResponse, CliError> {
        let (status, text) = self.send_json(Method::GET, &format!("/api/meetings/{code}"), None)?;
        if status != 200 {
            return Err(status_error(status, &text));
        }
        serde_json::from_str(&text)
            .map_err(|e| CliError::Server(format!("unreadable meeting response: {e}")))
    }

    /// DELETE /api/meetings/{code} (needs `meetings:write`).
    pub fn cancel_meeting(&mut self, code: &str) -> Result<(), CliError> {
        let (status, text) =
            self.send_json(Method::DELETE, &format!("/api/meetings/{code}"), None)?;
        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(status_error(status, &text))
    }

    /// GET /api/meetings/{code}/ics — the calendar file (public).
    pub fn fetch_ics(&self, code: &str) -> Result<String, CliError> {
        let (status, text) = self.get_public(&format!("/api/meetings/{code}/ics"))?;
        if status == 200 {
            return Ok(text);
        }
        Err(status_error(status, &text))
    }

    /// GET /api/auth/me — works with either a session cookie or a
    /// bearer access token.
    pub fn whoami(&mut self) -> Result<serde_json::Value, CliError> {
        let (status, text) = self.send_json(Method::GET, "/api/auth/me", None)?;
        if status != 200 {
            return Err(status_error(status, &text));
        }
        serde_json::from_str(&text)
            .map_err(|e| CliError::Server(format!("unreadable session response: {e}")))
    }
}

/// Turn a non-2xx response into the right CLI error: 401 is an auth
/// failure (exit 2); anything else is a server-semantic error (exit 3)
/// carrying the server's own message when we can read one.
pub fn status_error(status: u16, body: &str) -> CliError {
    let message = error_message(status, body);
    if status == 401 {
        CliError::Auth(message)
    } else {
        CliError::Server(message)
    }
}

fn error_message(status: u16, body: &str) -> String {
    if let Ok(api) = serde_json::from_str::<ApiError>(body) {
        return api.message;
    }
    if let Ok(oauth) = serde_json::from_str::<OAuthError>(body) {
        return match oauth.error_description {
            Some(detail) => format!("{}: {detail}", oauth.error),
            None => oauth.error,
        };
    }
    let body = body.trim();
    if body.is_empty() {
        format!("HTTP {status}")
    } else {
        body.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_shaped_errors_are_classified() {
        assert!(matches!(
            status_error(401, r#"{"code":"unauthorized","message":"no session"}"#),
            CliError::Auth(_)
        ));
        assert!(matches!(
            status_error(403, r#"{"code":"unauthorized","message":"scoped down"}"#),
            CliError::Server(_)
        ));
        assert_eq!(
            error_message(403, r#"{"code":"unauthorized","message":"scoped down"}"#),
            "scoped down"
        );
        assert_eq!(error_message(500, ""), "HTTP 500");
    }
}
