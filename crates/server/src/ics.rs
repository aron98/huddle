//! iCalendar (RFC 5545) export for a single meeting.
//!
//! The output is a minimal `VCALENDAR`/`VEVENT` pair: enough for calendar
//! clients to show the meeting with a join link. Long lines are not folded
//! (titles are short; clients tolerate unfolded lines in practice).

use time::macros::format_description;

use crate::meetings::Meeting;

/// Render `meeting` as an iCalendar document. `base_url` is the server's
/// public base; the join link points at `{base_url}/m/{code}`.
pub fn build_ics(meeting: &Meeting, base_url: &str) -> Result<String, time::error::Format> {
    let fmt = format_description!("[year][month][day]T[hour][minute][second]Z");
    let created = meeting.created_at.format(&fmt)?;
    let start = meeting.starts_at.format(&fmt)?;
    let end = meeting.ends_at.format(&fmt)?;

    let lines = [
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".to_owned(),
        "PRODID:-//huddle//scheduling//EN".to_owned(),
        "BEGIN:VEVENT".to_owned(),
        format!("UID:{meeting_id}@huddle", meeting_id = meeting.id),
        format!("DTSTAMP:{created}"),
        format!("DTSTART:{start}"),
        format!("DTEND:{end}"),
        format!("SUMMARY:{}", escape_ics_text(&meeting.title)),
        format!(
            "URL:{base}/m/{code}",
            base = base_url.trim_end_matches('/'),
            code = meeting.code
        ),
        "END:VEVENT".to_owned(),
        "END:VCALENDAR".to_owned(),
    ];
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
        let ics = build_ics(&meeting(), "http://localhost:3000/").expect("format succeeds");
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(ics.ends_with("END:VCALENDAR\r\n"));
        assert!(ics.contains("DTSTART:20260101T090000Z"));
        assert!(ics.contains("DTEND:20260101T100000Z"));
        assert!(ics.contains("SUMMARY:Team\\, sync\\; plans\\nv2"));
        assert!(ics.contains("URL:http://localhost:3000/m/ABCD-EFGH-JKLM"));
        assert!(ics.contains("UID:00000000-0000-0000-0000-000000000000@huddle"));
    }
}
