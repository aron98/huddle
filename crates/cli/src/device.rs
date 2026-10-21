//! RFC 8628 device-authorization login (issue #9): the human approves
//! in a browser, this process polls the token endpoint until the
//! server answers something other than "pending".

use std::{thread::sleep, time::Duration};

use huddle_protocol::{
    DEVICE_GRANT_TYPE, DeviceAuthorizationResponse, OAuthError, OAuthTokenResponse,
};

use crate::client::{CliError, Client, status_error};

/// The scopes the CLI asks for: both, so one login serves reading
/// (list/get) and writing (create/cancel).
pub const REQUESTED_SCOPES: &str = "meetings:read meetings:write";

/// The RFC 8628 step the operator must complete in a browser, plus the
/// poll parameters the token endpoint told us to honor.
pub struct LoginView {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub interval: u64,
}

/// Register the device and get the code the human must enter.
pub fn start(client: &Client) -> Result<LoginView, CliError> {
    let (status, body) = client.post_form(
        "/api/oauth/device/register",
        &[
            ("client_name", crate::client::CLIENT_NAME.to_owned()),
            ("scope", REQUESTED_SCOPES.to_owned()),
        ],
    )?;
    if status != 200 {
        return Err(status_error(status, &body));
    }
    let response: DeviceAuthorizationResponse = serde_json::from_str(&body)
        .map_err(|e| CliError::Server(format!("unreadable device registration: {e}")))?;
    Ok(LoginView {
        device_code: response.device_code,
        user_code: response.user_code,
        verification_uri: response.verification_uri,
        verification_uri_complete: response.verification_uri_complete,
        interval: u64::from(response.interval).max(1),
    })
}

/// Poll until the approval page answers. `authorization_pending` keeps
/// waiting, `slow_down` widens the interval by the RFC's five seconds,
/// and everything terminal becomes the matching CLI error.
pub fn wait_for_approval(
    client: &Client,
    device_code: String,
    mut interval: u64,
) -> Result<OAuthTokenResponse, CliError> {
    loop {
        sleep(Duration::from_secs(interval.max(1)));
        let (status, body) = client.post_form(
            "/api/oauth/token",
            &[
                ("grant_type", DEVICE_GRANT_TYPE.to_owned()),
                ("device_code", device_code.clone()),
            ],
        )?;
        if status == 200 {
            return serde_json::from_str(&body)
                .map_err(|e| CliError::Server(format!("unreadable token response: {e}")));
        }
        if status == 429 {
            // Back off politely rather than erroring: the login is
            // still perfectly alive behind the rate limit.
            interval += 5;
            continue;
        }
        let error = serde_json::from_str::<OAuthError>(&body)
            .ok()
            .map(|error| error.error);
        match error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => interval += 5,
            Some("access_denied") => {
                return Err(CliError::Auth(
                    "the request was denied on the approval page".to_owned(),
                ));
            }
            Some("expired_token") => {
                return Err(CliError::Auth(
                    "the device request expired before it was approved".to_owned(),
                ));
            }
            Some(other) => {
                return Err(CliError::Server(format!(
                    "token endpoint rejected the poll: {other}"
                )));
            }
            None => {
                return Err(CliError::Server(format!("HTTP {status}: {body}")));
            }
        }
    }
}
