//! Validated, normalized huddle server origins.
//!
//! The shell needs exactly one piece of user configuration: where the
//! server is. A [`ServerOrigin`] is the *origin* form (`scheme://host[:port]`)
//! with no path, query, or fragment — the web client can only run at a
//! server root because every REST call it makes is a same-origin
//! *absolute* path (`/api/...`, `/m/{code}`), and the WebSocket URL is
//! built from the page's own `location.host`. Deployments under a reverse
//!-proxy subpath are therefore not supported by the web client at all, and
//! this parser rejects them early with a clear message instead of letting
//! the webview produce a confusing 404.

use std::fmt;

/// Errors from [`ServerOrigin::parse`]. The strings are user-facing: the
/// settings UI surfaces them verbatim next to the server field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerUrlError {
    /// Empty (or whitespace-only) input.
    Empty,
    /// No `scheme://` prefix, e.g. `meet.example.com`.
    MissingScheme,
    /// Something other than `http`/`https`.
    UnsupportedScheme(String),
    /// Host missing or malformed.
    MissingHost,
    /// `/api`-style absolute paths are fine, but the *origin* itself must
    /// not carry one: `https://host/subpath` cannot serve the web client.
    OriginHasPath,
    /// `?query` / `#fragment` in a configured server address.
    OriginHasQueryOrFragment,
    /// Port is not a valid `u16`.
    InvalidPort(String),
    /// `user:pass@host` — credentials never make sense here.
    HasUserInfo,
}

impl fmt::Display for ServerUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the server address is empty"),
            Self::MissingScheme => write!(
                f,
                "the server address needs a scheme, like https://meet.example.com"
            ),
            Self::UnsupportedScheme(s) => {
                write!(f, "unsupported scheme {s:?}: use http:// or https://")
            }
            Self::MissingHost => write!(f, "the server address has no host"),
            Self::OriginHasPath => write!(
                f,
                "the server address must be a bare origin; huddle serves the app at its root"
            ),
            Self::OriginHasQueryOrFragment => {
                write!(f, "the server address must not contain a query or fragment")
            }
            Self::InvalidPort(p) => write!(f, "invalid port {p:?}"),
            Self::HasUserInfo => write!(f, "the server address must not contain credentials"),
        }
    }
}

impl std::error::Error for ServerUrlError {}

/// A validated huddle server origin, e.g. `https://meet.example.com:8443`.
///
/// Construct only via [`ServerOrigin::parse`]; the stored string is
/// normalized (lower-case scheme and host, default port dropped, no
/// trailing slash) so two spellings of the same server compare equal and
/// persist identically in [`crate::settings::Settings`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerOrigin {
    origin: String,
}

impl ServerOrigin {
    /// Parse and normalize a user-supplied server address.
    ///
    /// Accepted: `http://host`, `https://host:port`, IPv6 literals
    /// (`https://[::1]:8443`). Rejected: anything with userinfo, a path,
    /// a query, a fragment, or a non-HTTP(S) scheme — see
    /// [`ServerUrlError`] for the messages the settings UI shows.
    pub fn parse(input: &str) -> Result<Self, ServerUrlError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(ServerUrlError::Empty);
        }

        let (scheme, rest) = input
            .split_once("://")
            .ok_or(ServerUrlError::MissingScheme)?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(ServerUrlError::UnsupportedScheme(scheme));
        }

        // Strip path/query/fragment; each is rejected so the user gets a
        // precise complaint instead of a silently truncated origin. A
        // single trailing '/' on the root is "no path" (users type it,
        // browsers send it), so it is trimmed first.
        let rest = match rest.strip_suffix('/') {
            Some(stripped) if !stripped.contains(['/', '?', '#']) => stripped,
            _ => rest,
        };
        let authority = match rest.find(['/', '?', '#']) {
            Some(0) => return Err(ServerUrlError::MissingHost),
            Some(idx) if rest.as_bytes()[idx] == b'/' => return Err(ServerUrlError::OriginHasPath),
            Some(_) => return Err(ServerUrlError::OriginHasQueryOrFragment),
            None => rest,
        };
        if authority.contains(['?', '#']) {
            return Err(ServerUrlError::OriginHasQueryOrFragment);
        }
        if authority.contains('@') {
            return Err(ServerUrlError::HasUserInfo);
        }

        // Split host from port, honoring bracketed IPv6 literals where the
        // colons belong to the address, not the port separator.
        let (host, port_str) = if let Some(stripped) = authority.strip_prefix('[') {
            let end = stripped.find(']').ok_or(ServerUrlError::MissingHost)?;
            let host = format!("[{}]", &stripped[..end]);
            let after = &stripped[end + 1..];
            let port = after.strip_prefix(':').map(str::to_owned);
            (host, port)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), Some(p.to_owned())),
                None => (authority.to_owned(), None),
            }
        };
        if host.is_empty() || host == "[]" {
            return Err(ServerUrlError::MissingHost);
        }

        let mut port = None;
        if let Some(p) = port_str {
            let parsed: u16 = p
                .parse()
                .map_err(|_| ServerUrlError::InvalidPort(p.clone()))?;
            // Drop the default port for the scheme so `https://h:443` and
            // `https://h` are the same persisted value.
            let default_port =
                scheme == "https" && parsed == 443 || scheme == "http" && parsed == 80;
            if !default_port {
                port = Some(parsed);
            }
        }

        // Hostnames are case-insensitive; lowercase for canonical storage.
        let host = host.to_ascii_lowercase();
        let origin = match port {
            Some(p) => format!("{scheme}://{host}:{p}"),
            None => format!("{scheme}://{host}"),
        };
        Ok(Self { origin })
    }

    /// The normalized origin string, e.g. `https://meet.example.com`.
    pub fn as_str(&self) -> &str {
        &self.origin
    }

    /// An absolute URL for a server path (`path` must start with `/`).
    ///
    /// The web client's own routes and REST endpoints are all absolute
    /// paths at the origin root, so plain concatenation is exactly right
    /// (and stays testable without a URL crate).
    pub fn url(&self, path: &str) -> String {
        debug_assert!(path.starts_with('/'), "path must be absolute");
        format!("{}{}", self.origin, path)
    }

    /// The join page for a meeting code: `{origin}/m/{code}`.
    pub fn join_url(&self, code: &str) -> String {
        self.url(&format!("/m/{code}"))
    }

    /// The dashboard the organizer lands on — also the tray's fallback
    /// destination whenever a server-side API call cannot be authenticated
    /// (the session cookie lives in the webview, not in the shell's HTTP
    /// client; see ADR 014).
    pub fn dashboard_url(&self) -> String {
        self.url("/")
    }
}

impl fmt::Display for ServerOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes_plain_origins() {
        assert_eq!(
            ServerOrigin::parse("https://meet.example.com")
                .unwrap()
                .as_str(),
            "https://meet.example.com"
        );
        // Whitespace and case are normalized away.
        assert_eq!(
            ServerOrigin::parse("  HTTPS://Meet.Example.COM  ")
                .unwrap()
                .as_str(),
            "https://meet.example.com"
        );
        // A trailing slash on the root is "no path", not a subpath.
        assert_eq!(
            ServerOrigin::parse("https://meet.example.com/")
                .unwrap()
                .as_str(),
            "https://meet.example.com"
        );
    }

    #[test]
    fn keeps_explicit_nonstandard_ports() {
        assert_eq!(
            ServerOrigin::parse("http://localhost:3131")
                .unwrap()
                .as_str(),
            "http://localhost:3131"
        );
    }

    #[test]
    fn drops_default_ports() {
        assert_eq!(
            ServerOrigin::parse("https://meet.example.com:443")
                .unwrap()
                .as_str(),
            "https://meet.example.com"
        );
        assert_eq!(
            ServerOrigin::parse("http://meet.example.com:80")
                .unwrap()
                .as_str(),
            "http://meet.example.com"
        );
        // But not when it disagrees with the scheme.
        assert_eq!(
            ServerOrigin::parse("https://meet.example.com:80")
                .unwrap()
                .as_str(),
            "https://meet.example.com:80"
        );
    }

    #[test]
    fn parses_ipv6_literals() {
        assert_eq!(
            ServerOrigin::parse("https://[::1]:8443").unwrap().as_str(),
            "https://[::1]:8443"
        );
        assert_eq!(
            ServerOrigin::parse("http://[2001:db8::1]")
                .unwrap()
                .as_str(),
            "http://[2001:db8::1]"
        );
    }

    #[test]
    fn rejects_bad_input_with_specific_errors() {
        assert_eq!(ServerOrigin::parse(""), Err(ServerUrlError::Empty));
        assert_eq!(ServerOrigin::parse("   "), Err(ServerUrlError::Empty));
        assert_eq!(
            ServerOrigin::parse("meet.example.com"),
            Err(ServerUrlError::MissingScheme)
        );
        assert_eq!(
            ServerOrigin::parse("ftp://meet.example.com"),
            Err(ServerUrlError::UnsupportedScheme("ftp".to_owned()))
        );
        assert_eq!(
            ServerOrigin::parse("https://"),
            Err(ServerUrlError::MissingHost)
        );
        assert_eq!(
            ServerOrigin::parse("https:///api"),
            Err(ServerUrlError::MissingHost)
        );
        assert_eq!(
            ServerOrigin::parse("https://meet.example.com/huddle"),
            Err(ServerUrlError::OriginHasPath)
        );
        assert_eq!(
            ServerOrigin::parse("https://meet.example.com?a=1"),
            Err(ServerUrlError::OriginHasQueryOrFragment)
        );
        assert_eq!(
            ServerOrigin::parse("https://meet.example.com#top"),
            Err(ServerUrlError::OriginHasQueryOrFragment)
        );
        assert_eq!(
            ServerOrigin::parse("https://user:pass@meet.example.com"),
            Err(ServerUrlError::HasUserInfo)
        );
        assert_eq!(
            ServerOrigin::parse("https://meet.example.com:70000"),
            Err(ServerUrlError::InvalidPort("70000".to_owned()))
        );
        assert_eq!(
            ServerOrigin::parse("https://meet.example.com:abc"),
            Err(ServerUrlError::InvalidPort("abc".to_owned()))
        );
    }

    #[test]
    fn ipv6_without_closing_bracket_is_rejected() {
        assert_eq!(
            ServerOrigin::parse("https://[::1").unwrap_err(),
            ServerUrlError::MissingHost
        );
    }

    #[test]
    fn url_joins_absolute_paths() {
        let origin = ServerOrigin::parse("https://meet.example.com").unwrap();
        assert_eq!(
            origin.url("/api/health"),
            "https://meet.example.com/api/health"
        );
        assert_eq!(
            origin.join_url("H7KD-PM2X-QR4T"),
            "https://meet.example.com/m/H7KD-PM2X-QR4T"
        );
        assert_eq!(origin.dashboard_url(), "https://meet.example.com/");
    }

    #[test]
    fn equal_spellings_compare_equal() {
        let a = ServerOrigin::parse("https://meet.example.com:443").unwrap();
        let b = ServerOrigin::parse("HTTPS://Meet.Example.COM/").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), "https://meet.example.com");
    }
}
