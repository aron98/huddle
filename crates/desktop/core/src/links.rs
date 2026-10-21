//! Meeting-link handling for the desktop shell.
//!
//! "Deep handling" of a huddle join link means recognizing the shapes the
//! product actually produces and reducing them to a meeting code plus the
//! server it lives on:
//!
//! - a full join URL — `https://meet.example.com/m/H7KD-PM2X-QR4T` — also
//!   matches the `copy link` output of the web dashboard, and carries its
//!   own origin (link from a teammate ⇒ use *that* server, even if the
//!   shell was pointed elsewhere);
//! - a bare meeting code — `H7KD-PM2X-QR4T` — joined on the configured
//!   server.
//!
//! Codes mirror the server's alphabet (`crates/server/src/meetings.rs`):
//! 12 characters from `ABCDEFGHJKMNPQRSTUVWXYZ23456789` — ambiguous glyphs
//! (I, L, O, U, 0, 1) are excluded so codes stay typeable — grouped 4-4-4
//! with dashes.

use crate::server::{ServerOrigin, ServerUrlError};

/// Errors from [`parse_join_input`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinParseError {
    /// Empty (or whitespace-only) input.
    Empty,
    /// A URL, but not a huddle join page (`/m/{code}`).
    NotAMeetingUrl(String),
    /// The URL's origin failed validation.
    Server(ServerUrlError),
    /// Input looked like a bare code but is not a well-formed one.
    InvalidCode(String),
    /// A bare code with no configured server to join it on.
    NoDefaultServer,
}

impl std::fmt::Display for JoinParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "enter a meeting link or code"),
            Self::NotAMeetingUrl(url) => {
                write!(f, "{url} is not a huddle meeting link (expected …/m/CODE)")
            }
            Self::Server(e) => write!(f, "invalid server in link: {e}"),
            Self::InvalidCode(c) => write!(f, "{c:?} is not a valid meeting code"),
            Self::NoDefaultServer => write!(f, "configure a server first, then join by code"),
        }
    }
}

impl std::error::Error for JoinParseError {}

/// A parsed join target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinTarget {
    /// The meeting code, upper-cased and dash-grouped.
    pub code: String,
    /// The server the meeting lives on: taken from a full link, or the
    /// `default_origin` for a bare code.
    pub origin: ServerOrigin,
}

/// The server code alphabet (see module docs) — digits 2-9 and the
/// unambiguous uppercase letters.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 12;

/// Whether a bare string is a well-formed huddle meeting code
/// (`XXXX-XXXX-XXXX`), ignoring case and surrounding whitespace.
fn is_valid_code(normalized: &str) -> bool {
    let bytes = normalized.as_bytes();
    // 12 glyphs plus the two grouping dashes.
    normalized.len() == CODE_LEN + 2
        && bytes[4] == b'-'
        && bytes[9] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 9 || CODE_ALPHABET.contains(b))
}

/// Normalize a code as typed: trim, and the server is case-insensitive
/// about codes (`normalize_code` in `crates/server/src/meetings.rs`) so
/// upper-case for canonical display.
pub fn normalize_code(raw: &str) -> String {
    raw.trim().to_ascii_uppercase()
}

/// Parse user input on the "join" field: a full join URL or a bare code.
///
/// `default_origin` is the configured server, used only for bare codes.
pub fn parse_join_input(
    input: &str,
    default_origin: Option<&ServerOrigin>,
) -> Result<JoinTarget, JoinParseError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(JoinParseError::Empty);
    }

    // A full URL: validate the origin part through ServerOrigin and require
    // the join-page path. Leading text without "://" is never a URL.
    if let Some(idx) = input.find("://") {
        // `prefix` is "scheme://" — everything the origin parser needs
        // before the first '/' of the path.
        let (prefix, rest) = input.split_at(idx + 3);
        let (authority, path) = match rest.find('/') {
            Some(path_idx) => rest.split_at(path_idx),
            None => (rest, ""),
        };
        let origin =
            ServerOrigin::parse(&format!("{prefix}{authority}")).map_err(JoinParseError::Server)?;
        let code = path
            .strip_prefix("/m/")
            .map(normalize_code)
            .ok_or_else(|| JoinParseError::NotAMeetingUrl(input.to_owned()))?;
        if !is_valid_code(&code) {
            return Err(JoinParseError::InvalidCode(code));
        }
        return Ok(JoinTarget { code, origin });
    }

    // Otherwise: a bare meeting code, joined on the configured server.
    let code = normalize_code(input);
    if !is_valid_code(&code) {
        return Err(JoinParseError::InvalidCode(input.trim().to_owned()));
    }
    match default_origin {
        Some(origin) => Ok(JoinTarget {
            code,
            origin: origin.clone(),
        }),
        None => Err(JoinParseError::NoDefaultServer),
    }
}

/// Build the join URL for a code on an origin: `{origin}/m/{code}`.
pub fn build_join_url(origin: &ServerOrigin, code: &str) -> String {
    origin.join_url(&normalize_code(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> ServerOrigin {
        ServerOrigin::parse("https://meet.example.com").unwrap()
    }

    #[test]
    fn parses_a_full_join_url_with_its_own_origin() {
        let t = parse_join_input("https://other.example.com/m/H7KD-PM2X-QR4T", None).unwrap();
        // The link's server wins even over a configured default.
        assert_eq!(t.code, "H7KD-PM2X-QR4T");
        assert_eq!(t.origin.as_str(), "https://other.example.com");
    }

    #[test]
    fn parses_a_bare_code_against_the_default_server() {
        let o = origin();
        let t = parse_join_input("h7kd-pm2x-qr4t", Some(&o)).unwrap();
        assert_eq!(t.code, "H7KD-PM2X-QR4T");
        assert_eq!(t.origin, o);
        // Trimming is applied.
        let t = parse_join_input("  H7KD-PM2X-QR4T  ", Some(&o)).unwrap();
        assert_eq!(t.code, "H7KD-PM2X-QR4T");
    }

    #[test]
    fn url_with_port_and_lowercase_code_normalizes() {
        let t = parse_join_input("http://localhost:3131/m/h7kd-pm2x-qr4t", None).unwrap();
        assert_eq!(t.code, "H7KD-PM2X-QR4T");
        assert_eq!(t.origin.as_str(), "http://localhost:3131");
    }

    #[test]
    fn rejects_a_url_that_is_not_a_join_page() {
        assert_eq!(
            parse_join_input("https://meet.example.com/dashboard", None).unwrap_err(),
            JoinParseError::NotAMeetingUrl("https://meet.example.com/dashboard".to_owned())
        );
        assert_eq!(
            parse_join_input("https://meet.example.com/", None).unwrap_err(),
            JoinParseError::NotAMeetingUrl("https://meet.example.com/".to_owned())
        );
        // A join path with a bogus code is an invalid code, not a wrong URL.
        assert_eq!(
            parse_join_input("https://meet.example.com/m/SHORT", None).unwrap_err(),
            JoinParseError::InvalidCode("SHORT".to_owned())
        );
    }

    #[test]
    fn rejects_invalid_codes() {
        // Ambiguous glyphs are not in the alphabet.
        assert!(matches!(
            parse_join_input("H7KO-PM2X-QR4T", Some(&origin())),
            Err(JoinParseError::InvalidCode(_))
        ));
        // Too short.
        assert!(matches!(
            parse_join_input("H7KD-PM2X", Some(&origin())),
            Err(JoinParseError::InvalidCode(_))
        ));
        // Too long.
        assert!(matches!(
            parse_join_input("H7KD-PM2X-QR4T-XXXX", Some(&origin())),
            Err(JoinParseError::InvalidCode(_))
        ));
        // Dashes in the wrong places.
        assert!(matches!(
            parse_join_input("H7KDPM2X-QR4T", Some(&origin())),
            Err(JoinParseError::InvalidCode(_))
        ));
    }

    #[test]
    fn bare_code_without_a_configured_server_is_a_clear_error() {
        assert_eq!(
            parse_join_input("H7KD-PM2X-QR4T", None).unwrap_err(),
            JoinParseError::NoDefaultServer
        );
    }

    #[test]
    fn empty_input_is_rejected() {
        assert_eq!(
            parse_join_input("", None).unwrap_err(),
            JoinParseError::Empty
        );
        assert_eq!(
            parse_join_input("   ", Some(&origin())).unwrap_err(),
            JoinParseError::Empty
        );
    }

    #[test]
    fn join_urls_are_built_normalized() {
        assert_eq!(
            build_join_url(&origin(), "h7kd-pm2x-qr4t"),
            "https://meet.example.com/m/H7KD-PM2X-QR4T"
        );
    }

    #[test]
    fn url_with_bad_origin_reports_the_server_error() {
        // No port digits.
        assert!(matches!(
            parse_join_input("https://meet.example.com:abc/m/H7KD-PM2X-QR4T", None),
            Err(JoinParseError::Server(_))
        ));
    }
}
