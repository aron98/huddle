//! Same-origin REST helpers for the auth and dashboard pages.
//!
//! Thin `fetch` wrappers: the browser attaches session cookies to
//! same-origin requests by default, so no cookie plumbing is needed here.

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestInit, Response};

/// Send a same-origin request and return `(status, body)`.
pub async fn fetch_text(
    method: &str,
    path: &str,
    body: Option<String>,
) -> Result<(u16, String), String> {
    let init = RequestInit::new();
    init.set_method(method);
    if let Some(body) = &body {
        init.set_body(&wasm_bindgen::JsValue::from_str(body));
        let headers = Headers::new().map_err(|e| format!("cannot create headers: {e:?}"))?;
        headers
            .set("content-type", "application/json")
            .map_err(|e| format!("cannot set content-type: {e:?}"))?;
        init.set_headers(&headers);
    }

    let request = Request::new_with_str_and_init(path, &init)
        .map_err(|e| format!("cannot build request for {path}: {e:?}"))?;

    let promise = web_sys::window()
        .ok_or_else(|| "no window".to_owned())?
        .fetch_with_request(&request);
    let response = JsFuture::from(promise)
        .await
        .map_err(|e| format!("fetch failed: {e:?}"))?
        .dyn_into::<Response>()
        .map_err(|e| format!("fetch returned {e:?}"))?;

    let status = response.status();
    let text = match response.text() {
        Ok(promise) => JsFuture::from(promise)
            .await
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    Ok((status, text))
}

/// Convert a `datetime-local` input value ("2026-10-14T18:30", local wall
/// time) into an RFC3339 UTC timestamp the API accepts. `None` when the
/// browser cannot parse the value.
pub fn local_datetime_to_rfc3339(value: &str) -> Option<String> {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(value));
    if date.get_time().is_nan() {
        return None;
    }
    date.to_iso_string().as_string()
}

/// Render an RFC3339 UTC timestamp in the viewer's local time — used for
/// "starts at" and "ends at" hints. Falls back to the raw string when the
/// browser cannot parse it.
pub fn rfc3339_to_local(value: &str) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(value));
    if date.get_time().is_nan() {
        return value.to_owned();
    }
    // Invoke `toLocaleString(undefined)` through reflection: js-sys's
    // typed wrapper only accepts a `&str` locale and Chrome rejects `""`
    // as an invalid language tag, while `undefined` selects the viewer's
    // own locale.
    js_sys::Reflect::get(
        date.as_ref(),
        &wasm_bindgen::JsValue::from_str("toLocaleString"),
    )
    .ok()
    .and_then(|method| method.dyn_into::<js_sys::Function>().ok())
    .and_then(|method| {
        method
            .call1(date.as_ref(), &wasm_bindgen::JsValue::UNDEFINED)
            .ok()
    })
    .and_then(|formatted| formatted.as_string())
    .unwrap_or_else(|| value.to_owned())
}

/// Extracts the single-sign-on entry point from a magic-link refusal
/// body, when the server is configured for OIDC sign-in (issue #89).
///
/// The server answers `POST /api/auth/request-link` with HTTP 409 and
/// `{"login":"/api/auth/oidc/login","status":"oidc_configured"}` when
/// SSO replaces magic links; the login page follows `login` instead of
/// surfacing the raw refusal body to the user.
pub fn oidc_login_redirect(body: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    if value.get("status").and_then(|status| status.as_str()) == Some("oidc_configured") {
        value
            .get("login")
            .and_then(|login| login.as_str())
            .map(str::to_owned)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::oidc_login_redirect;

    #[test]
    fn oidc_configured_refusal_yields_the_sso_entry_point() {
        let body = r#"{"login":"/api/auth/oidc/login","status":"oidc_configured"}"#;
        assert_eq!(
            oidc_login_redirect(body),
            Some("/api/auth/oidc/login".to_owned())
        );
    }

    #[test]
    fn other_refusal_bodies_yield_none() {
        assert_eq!(oidc_login_redirect(r#"{"code":"rate_limited"}"#), None);
        assert_eq!(oidc_login_redirect("not json"), None);
    }
}
