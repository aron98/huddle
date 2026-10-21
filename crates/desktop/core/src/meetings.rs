//! The scheduling-API surface the tray uses: request URLs, response
//! parsing, and next-meeting selection.
//!
//! The wire shapes mirror `crates/protocol/src/api.rs` (`MeetingResponse`
//! and friends) — deliberately *leniently*: the shell parses only the
//! fields it needs and defaults the rest, so protocol additions never
//! break the tray. The shell cannot depend on the `huddle-protocol` crate
//! itself: it lives in the root workspace, whose members resolve their
//! dependencies through `[workspace.dependencies]`, and this workspace is
//! deliberately standalone (ADR 014).
//!
//! A caveat documented in ADR 014 and honestly wired in the app: the
//! tray's HTTP client has *no session credentials* (the magic-link session
//! cookie lives in the webview's cookie jar, which the shell cannot read).
//! `/api/meetings/mine` is session-gated, so today the fetch comes back
//! 401 and the tray falls back to opening the dashboard, where the
//! webview's own logged-in page lists meetings with join buttons. The
//! pure selection logic below is ready for a future non-browser token.

use std::time::Duration;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::server::ServerOrigin;

/// Timeout for the tray's server-side API calls. The tray is a convenience
/// surface; it must never hang the menu — the dashboard fallback is always
/// one click away.
pub const API_TIMEOUT: Duration = Duration::from_secs(5);

/// The subset of `protocol::MeetingDto` the shell needs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MeetingCore {
    pub code: String,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(default)]
    pub cancelled: bool,
}

/// The subset of `protocol::OccurrenceDto` the shell needs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OccurrenceCore {
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(default)]
    pub cancelled: bool,
}

/// The subset of `protocol::MeetingResponse` the shell needs — one row of
/// `GET /api/meetings/mine`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct MeetingRow {
    pub meeting: MeetingCore,
    #[serde(default)]
    pub upcoming_occurrences: Vec<OccurrenceCore>,
}

/// `GET {origin}/api/health` — the reachability probe the settings page
/// runs before connecting.
pub fn health_endpoint(origin: &ServerOrigin) -> String {
    origin.url("/api/health")
}

/// `GET {origin}/api/meetings/mine` — the organizer's meeting list (the
/// same endpoint and shapes the web dashboard fetches via
/// `crates/web/src/api.rs`).
pub fn mine_endpoint(origin: &ServerOrigin) -> String {
    origin.url("/api/meetings/mine")
}

/// Parse the `/api/meetings/mine` response body.
pub fn parse_mine_response(body: &str) -> Result<Vec<MeetingRow>, serde_json::Error> {
    serde_json::from_str(body)
}

/// The next meeting worth joining, as the tray's "Join next meeting" sees it.
///
/// Rule: across all non-cancelled meetings, the earliest occurrence that
/// has not yet started (`starts_at >= now`); recurring series contribute
/// their `upcoming_occurrences`, one-offs their own `starts_at`. A meeting
/// that started earlier today is *not* "next" — the joinable-in-progress
/// case is the dashboard's business (the web UI shows it), while the tray
/// item answers "what is coming up". `None` when nothing is upcoming.
pub fn next_join_target(
    rows: &[MeetingRow],
    now: OffsetDateTime,
) -> Option<(String, OffsetDateTime)> {
    rows.iter()
        .filter(|row| !row.meeting.cancelled)
        .flat_map(|row| {
            // A series' first occurrence is its own `starts_at`, so a
            // one-off and a series' first date behave identically here.
            let first = std::iter::once(row.meeting.starts_at);
            let rest = row
                .upcoming_occurrences
                .iter()
                .filter(|o| !o.cancelled)
                .map(|o| o.starts_at);
            first
                .chain(rest)
                .map(move |starts_at| (row.meeting.code.clone(), starts_at))
        })
        .filter(|(_, starts_at)| *starts_at >= now)
        .min_by_key(|(_, starts_at)| *starts_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::ext::NumericalDuration;

    fn row(
        code: &str,
        starts_at: OffsetDateTime,
        occurrences: &[(OffsetDateTime, bool)],
    ) -> MeetingRow {
        MeetingRow {
            meeting: MeetingCore {
                code: code.to_owned(),
                starts_at,
                cancelled: false,
            },
            upcoming_occurrences: occurrences
                .iter()
                .map(|(starts_at, cancelled)| OccurrenceCore {
                    starts_at: *starts_at,
                    cancelled: *cancelled,
                })
                .collect(),
        }
    }

    #[test]
    fn endpoints_are_origin_absolute() {
        let origin = ServerOrigin::parse("https://meet.example.com").unwrap();
        assert_eq!(
            health_endpoint(&origin),
            "https://meet.example.com/api/health"
        );
        assert_eq!(
            mine_endpoint(&origin),
            "https://meet.example.com/api/meetings/mine"
        );
    }

    #[test]
    fn parses_a_real_shaped_payload_leniently() {
        // Extra protocol fields (title, has_passcode, joinability, …) are
        // ignored, and omitted optional fields default — the tray survives
        // protocol additions without a rebuild.
        let body = r#"[
            {
                "meeting": {
                    "id": "00000000-0000-0000-0000-000000000000",
                    "code": "H7KD-PM2X-QR4T",
                    "title": "Standup",
                    "starts_at": "2026-10-21T09:00:00Z",
                    "ends_at": "2026-10-21T09:30:00Z",
                    "cancelled": false,
                    "has_passcode": true
                },
                "joinability": {"type": "open"},
                "upcoming_occurrences": [
                    {"starts_at": "2026-10-22T09:00:00Z", "ends_at": "2026-10-22T09:30:00Z", "cancelled": false}
                ]
            }
        ]"#;
        let rows = parse_mine_response(body).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].meeting.code, "H7KD-PM2X-QR4T");
        assert_eq!(rows[0].upcoming_occurrences.len(), 1);
        assert!(!rows[0].meeting.cancelled);
    }

    #[test]
    fn legacy_payload_without_recurrence_still_parses() {
        let body = r#"[{"meeting": {"code": "H7KD-PM2X-QR4T", "starts_at": "2026-10-21T09:00:00Z", "cancelled": false}, "joinability": {"type": "open"}}]"#;
        let rows = parse_mine_response(body).unwrap();
        assert!(rows[0].upcoming_occurrences.is_empty());
    }

    #[test]
    fn picks_the_earliest_upcoming_occurrence() {
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let rows = vec![
            row("CCCC-CCCC-CCCC", now + 3.hours(), &[]),
            row("AAAA-AAAA-AAAA", now + 1.hours(), &[]),
            row("BBBB-BBBB-BBBB", now + 2.hours(), &[]),
        ];
        assert_eq!(
            next_join_target(&rows, now).map(|(c, _)| c),
            Some("AAAA-AAAA-AAAA".to_owned())
        );
    }

    #[test]
    fn includes_recurring_series_occurrences() {
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let rows = vec![row(
            "SERI-OUSS-ERIE",
            now - 24.hours(),
            &[
                (now + 2.hours(), false),
                (now + 1.hours(), false),
                (now + 30.minutes(), true), // cancelled single date
            ],
        )];
        // The series started yesterday; its next live occurrence is picked.
        assert_eq!(
            next_join_target(&rows, now),
            Some(("SERI-OUSS-ERIE".to_owned(), now + 1.hours()))
        );
    }

    #[test]
    fn skips_past_and_cancelled_and_finds_none_when_nothing_is_upcoming() {
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let past = vec![row("PAST-PAST-PAST", now - 1.hours(), &[])];
        assert_eq!(next_join_target(&past, now), None);

        let cancelled = vec![MeetingRow {
            meeting: MeetingCore {
                code: "CANC-ELLE-DMEG".to_owned(),
                starts_at: now + 1.hours(),
                cancelled: true,
            },
            upcoming_occurrences: vec![],
        }];
        assert_eq!(next_join_target(&cancelled, now), None);

        // An in-progress meeting is not "next": it already started.
        let ongoing = vec![row("ONGO-INGM-EET1", now - 10.minutes(), &[])];
        assert_eq!(next_join_target(&ongoing, now), None);

        // Empty list: nothing upcoming.
        assert_eq!(next_join_target(&[], now), None);
    }

    #[test]
    fn exactly_now_counts_as_upcoming() {
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let rows = vec![row("STRT-INGR-IGHT", now, &[])];
        assert_eq!(
            next_join_target(&rows, now),
            Some(("STRT-INGR-IGHT".to_owned(), now))
        );
    }

    #[test]
    fn timeout_is_short() {
        assert!(API_TIMEOUT <= Duration::from_secs(10));
    }
}
