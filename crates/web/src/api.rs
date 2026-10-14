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
