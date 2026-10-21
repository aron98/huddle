//! iCalendar (RFC 5545) export for a single meeting.
//!
//! The output is a minimal `VCALENDAR`/`VEVENT` pair: enough for calendar
//! clients to show the meeting with a join link — and, for a recurring
//! series, the `RRULE` and `EXDATE` properties to expand and hole it.
//! Long lines are not folded (titles are short; clients tolerate
//! unfolded lines in practice).

use time::OffsetDateTime;
use time::macros::format_description;

use crate::meetings::Meeting;
use crate::recurrence;

/// Render `meeting` as an iCalendar document. `cancelled` holds the
/// starts of cancelled occurrences of a series (empty for one-offs);
/// they become an `EXDATE` property so imported calendars hole out
/// exactly the dates the organizer cancelled. `base_url` is the
/// server's public base; the join link points at `{base_url}/m/{code}`.
pub fn build_ics(
    meeting: &Meeting,
    cancelled: &[OffsetDateTime],
    base_url: &str,
) -> Result<String, time::error::Format> {
    let fmt = format_description!("[year][month][day]T[hour][minute][second]Z");
    let created = meeting.created_at.format(&fmt)?;
    let start = meeting.starts_at.format(&fmt)?;
    let end = meeting.ends_at.format(&fmt)?;

    let mut lines = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".to_owned(),
        "PRODID:-//huddle//scheduling//EN".to_owned(),
        "BEGIN:VEVENT".to_owned(),
        format!("UID:{meeting_id}@huddle", meeting_id = meeting.id),
        format!("DTSTAMP:{created}"),
        // DTSTART is the series start: the first occurrence, which the
        // RRULE expands from.
        format!("DTSTART:{start}"),
        format!("DTEND:{end}"),
    ];
    if let Some(rule) = &meeting.repeat {
        lines.push(format!("RRULE:{}", recurrence::rrule_property(rule)?));
    }
    if !cancelled.is_empty() {
        let dates = cancelled
            .iter()
            .map(|occurrence| recurrence::ics_datetime(*occurrence))
            .collect::<Result<Vec<_>, _>>()?;
        lines.push(format!("EXDATE:{}", dates.join(",")));
    }
    // Issue #29: calendar software cannot decrypt client-side encrypted
    // titles, so the export degrades honestly to the masked label
    // instead of leaking the ciphertext blob into users' calendars.
    lines.push(format!(
        "SUMMARY:{}",
        escape_ics_text(huddle_protocol::display_title(&meeting.title))
    ));
    lines.push(format!(
        "URL:{base}/m/{code}",
        base = base_url.trim_end_matches('/'),
        code = meeting.code
    ));
    lines.push("END:VEVENT".to_owned());
    lines.push("END:VCALENDAR".to_owned());
    Ok(lines.join("\r\n") + "\r\n")
}

/// Escape one TEXT property value per RFC 5545 §3.3.11: backslash,
/// semicolon, comma, and line breaks are backslash-escaped.
pub fn escape_ics_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {} // fold CRLF/LF into a single escaped newline
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use time::macros::datetime;

    fn meeting() -> Meeting {
        Meeting {
            id: uuid::Uuid::nil(),
            code: "ABCD-EFGH-JKLM".to_owned(),
            title: "Team, sync; plans\nv2".to_owned(),
            starts_at: datetime!(2026-01-01 09:00 UTC),
            ends_at: datetime!(2026-01-01 10:00 UTC),
            organizer_id: None,
            cancelled: false,
            created_at: datetime!(2025-12-30 12:00 UTC),
            repeat: None,
            passcode: None,
            e2ee: false,
        }
    }

    fn recurring_meeting() -> Meeting {
        Meeting {
            title: "Weekly sync".to_owned(),
            repeat: Some(huddle_protocol::RecurrenceRule {
                frequency: huddle_protocol::RepeatFrequency::Weekly,
                until: None,
                count: Some(5),
            }),
            ..meeting()
        }
    }

    #[test]
    fn text_is_escaped_per_rfc_5545() {
        assert_eq!(
            escape_ics_text("Team, sync; plans\nv2"),
            "Team\\, sync\\; plans\\nv2"
        );
        assert_eq!(escape_ics_text("plain"), "plain");
        assert_eq!(escape_ics_text("back\\slash"), "back\\\\slash");
        // CRLF collapses to one escaped newline.
        assert_eq!(escape_ics_text("a\r\nb"), "a\\nb");
    }

    #[test]
    fn timestamps_use_the_ics_utc_format() {
        let fmt = format_description!("[year][month][day]T[hour][minute][second]Z");
        let rendered = datetime!(2026-01-01 09:00 UTC)
            .format(&fmt)
            .expect("formatting cannot fail");
        assert_eq!(rendered, "20260101T090000Z");
    }

    #[test]
    fn built_calendar_has_the_meeting() {
        let ics = build_ics(&meeting(), &[], "http://localhost:3000/").expect("format succeeds");
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(ics.ends_with("END:VCALENDAR\r\n"));
        assert!(ics.contains("DTSTART:20260101T090000Z"));
        assert!(ics.contains("DTEND:20260101T100000Z"));
        assert!(ics.contains("SUMMARY:Team\\, sync\\; plans\\nv2"));
        assert!(ics.contains("URL:http://localhost:3000/m/ABCD-EFGH-JKLM"));
        assert!(ics.contains("UID:00000000-0000-0000-0000-000000000000@huddle"));
        // A one-off exports no recurrence properties at all.
        assert!(!ics.contains("RRULE:"));
        assert!(!ics.contains("EXDATE:"));
    }

    #[test]
    fn built_calendar_exports_the_recurrence() {
        let ics =
            build_ics(&recurring_meeting(), &[], "http://localhost:3000").expect("format succeeds");
        assert!(ics.contains("RRULE:FREQ=WEEKLY;COUNT=5"));
    }

    #[test]
    fn cancelled_occurrences_hole_out_the_series() {
        let cancelled = [
            datetime!(2026-01-08 09:00 UTC),
            datetime!(2026-01-22 09:00 UTC),
        ];
        let ics = build_ics(&recurring_meeting(), &cancelled, "http://localhost:3000")
            .expect("format succeeds");
        assert!(
            ics.contains("EXDATE:20260108T090000Z,20260122T090000Z"),
            "cancelled occurrences become one EXDATE list: {ics}"
        );
    }
}
