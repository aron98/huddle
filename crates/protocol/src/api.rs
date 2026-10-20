//! REST API DTOs for the scheduling surface (`/api/meetings`).
//!
//! These are shared between the server and the web client, same rationale as
//! the signaling types: one definition, compiled into both sides.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeetingDto {
    pub id: Uuid,
    pub code: String,
    pub title: String,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ends_at: OffsetDateTime,
    pub cancelled: bool,
    /// The recurrence rule when this meeting is a series, `None` for a
    /// one-off. `starts_at` is the first occurrence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<RecurrenceRule>,
    /// Whether joining requires a passcode. The passcode value itself is
    /// never sent over the wire — only this flag is.
    #[serde(default)]
    pub has_passcode: bool,
}

/// Curated recurrence frequencies for recurring meetings (issue 16). A
/// deliberately closed set — a full RFC 5545 editor is a rabbit hole;
/// every rule we can express round-trips through the ICS export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatFrequency {
    Daily,
    Weekly,
    /// Every weekday (Mon–Fri), weekends skipped.
    Weekdays,
    Monthly,
}

/// How a meeting series repeats. The meeting's `starts_at` is the first
/// occurrence (RFC 5545 `DTSTART`); every occurrence keeps the
/// meeting's duration. At most one of `until`/`count` may be set —
/// RFC 5545 §3.3.10 makes them mutually exclusive parts of one rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RecurrenceRule {
    pub frequency: RepeatFrequency,
    /// Last occurrence (inclusive), as a UTC timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(with = "time::serde::rfc3339::option")]
    pub until: Option<OffsetDateTime>,
    /// Total number of occurrences, counting the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

impl RecurrenceRule {
    /// Pure validation, shared by server and client so the form shows
    /// the same errors the API would return (same rationale as
    /// `CreateMeetingRequest::validate`). `series_start` is the first
    /// occurrence.
    pub fn validate(&self, series_start: OffsetDateTime) -> Result<(), &'static str> {
        if self.until.is_some() && self.count.is_some() {
            return Err("a recurrence may end with either a date or a count, not both");
        }
        if let Some(count) = self.count
            && count == 0
        {
            return Err("a recurrence count must be at least 1");
        }
        if let Some(until) = self.until
            && until <= series_start
        {
            return Err("a recurrence must include the first occurrence");
        }
        Ok(())
    }
}

/// One occurrence of a recurring meeting: a concrete start/end pair
/// expanded from the series rule, plus that occurrence's own cancel
/// state (cancelling one date leaves the series intact).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OccurrenceDto {
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ends_at: OffsetDateTime,
    pub cancelled: bool,
}

/// Whether a meeting may be joined right now. The join window is
/// `[starts_at - lead, ends_at + grace]`, with both margins configured on
/// the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Joinability {
    Open,
    #[serde(rename_all = "snake_case")]
    NotYet {
        #[serde(with = "time::serde::rfc3339")]
        opens_at: OffsetDateTime,
    },
    /// The join window (ends_at + grace) is in the past.
    Ended,
    /// The organizer cancelled the meeting.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateMeetingRequest {
    pub title: String,
    #[serde(with = "time::serde::rfc3339")]
    pub starts_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ends_at: OffsetDateTime,
    /// Make the meeting a recurring series instead of a one-off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<RecurrenceRule>,
    /// Optional passcode required to join the meeting. Transported in
    /// plain text (TLS carries the connection); the server compares it
    /// in constant time. Absent or `null` means no passcode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passcode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UpdateMeetingRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(with = "time::serde::rfc3339::option")]
    pub starts_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(with = "time::serde::rfc3339::option")]
    pub ends_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
    /// `None` (absent) leaves the recurrence unchanged; `Some(None)`
    /// (JSON `null`) clears it back to a one-off; `Some(Some(rule))`
    /// replaces the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "double_option")]
    pub repeat: Option<Option<RecurrenceRule>>,
    /// Set, change, or remove the join passcode: absent leaves it
    /// unchanged, `null` removes it, a string sets it.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub passcode: Option<Option<String>>,
}

/// Distinguish "field absent" (`None`) from "field present but `null`"
/// (`Some(None)`) when deserializing `Option<Option<T>>` — the standard
/// serde recipe, so a PATCH can clear a field by sending `null`.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MeetingResponse {
    pub meeting: MeetingDto,
    pub joinability: Joinability,
    /// Concrete upcoming occurrences of a recurring series (empty for
    /// one-off meetings), computed relative to the request time and
    /// marked with their per-occurrence cancel state.
    #[serde(default)]
    pub upcoming_occurrences: Vec<OccurrenceDto>,
}

/// A recording of one meeting, as tracked by the scheduling API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RecordingDto {
    pub id: Uuid,
    pub meeting_id: Uuid,
    /// The organizer who started the recording; `None` for meetings without
    /// an organizer (anonymous owner path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub stopped_at: Option<OffsetDateTime>,
    /// Total bytes written to disk once finalized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// True between start and stop.
    pub active: bool,
}

/// Error envelope for all REST endpoints: `{"error": {"code", "message"}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ApiError {
    pub code: crate::signaling::ErrorCode,
    pub message: String,
}

impl CreateMeetingRequest {
    /// Pure validation, shared by server and client so the form can show
    /// the same errors the API would return.
    pub fn validate(&self) -> Result<(), &'static str> {
        let title = self.title.trim();
        if title.is_empty() {
            return Err("title must not be empty");
        }
        if title.len() > 200 {
            return Err("title must be at most 200 characters");
        }
        if self.ends_at <= self.starts_at {
            return Err("ends_at must be after starts_at");
        }
        if let Some(rule) = &self.repeat {
            rule.validate(self.starts_at)?;
        }
        if let Some(passcode) = &self.passcode {
            validate_passcode(passcode)?;
        }
        Ok(())
    }
}

/// Maximum passcode length, shared by create and update validation.
pub const PASSCODE_MAX_LEN: usize = 128;

/// Validate a passcode value: trimmed, non-empty, bounded length.
/// Shared by the create request and the server's update path so both
/// agree on what a usable passcode is.
pub fn validate_passcode(passcode: &str) -> Result<(), &'static str> {
    let trimmed = passcode.trim();
    if trimmed.is_empty() {
        return Err("passcode must not be empty");
    }
    if trimmed.len() > PASSCODE_MAX_LEN {
        return Err("passcode must be at most 128 characters");
    }
    Ok(())
}

/// Serde for the tri-state `Option<Option<T>>`: an absent field means
/// "leave unchanged", an explicit `null` means "remove", and a value
/// means "set". A plain `Option<T>` cannot tell absent from null.
mod double_option {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<Option<String>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(inner) => serializer.serialize_some(inner),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        // The field is present (serde already checked that), so a `null`
        // inside it is the explicit "remove" request: `Some(None)`.
        Ok(Some(Option::<String>::deserialize(deserializer)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joinability_not_yet_shape_is_stable() {
        let j = Joinability::NotYet {
            opens_at: OffsetDateTime::UNIX_EPOCH,
        };
        let v = serde_json::to_value(&j).unwrap();
        assert_eq!(v["type"], "not_yet");
        assert_eq!(v["opens_at"], "1970-01-01T00:00:00Z");
        let back: Joinability = serde_json::from_value(v).unwrap();
        assert_eq!(back, j);
    }

    #[test]
    fn create_request_rejects_bad_input() {
        let req = CreateMeetingRequest {
            title: "  ".into(),
            starts_at: OffsetDateTime::UNIX_EPOCH,
            ends_at: OffsetDateTime::UNIX_EPOCH,
            repeat: None,
            passcode: None,
        };
        assert_eq!(req.validate(), Err("title must not be empty"));
    }

    #[test]
    fn create_request_rejects_backwards_times() {
        let req = CreateMeetingRequest {
            title: "Standup".into(),
            starts_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(2),
            ends_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            repeat: None,
            passcode: None,
        };
        assert_eq!(req.validate(), Err("ends_at must be after starts_at"));
    }

    #[test]
    fn recurrence_rule_round_trips() {
        let json = r#"{"frequency":"weekdays"}"#;
        let rule: RecurrenceRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.frequency, RepeatFrequency::Weekdays);
        assert_eq!(rule.until, None);
        assert_eq!(rule.count, None);
        // Optional parts are omitted, not serialized as null.
        assert_eq!(serde_json::to_string(&rule).unwrap(), json);

        let bounded = r#"{"frequency":"weekly","count":10}"#;
        let rule: RecurrenceRule = serde_json::from_str(bounded).unwrap();
        assert_eq!(rule.count, Some(10));
        assert_eq!(serde_json::to_string(&rule).unwrap(), bounded);
    }

    #[test]
    fn recurrence_rule_validation_is_shared() {
        let start = OffsetDateTime::UNIX_EPOCH + time::Duration::days(1);
        let rule = |until, count| RecurrenceRule {
            frequency: RepeatFrequency::Daily,
            until,
            count,
        };
        assert_eq!(
            rule(Some(start + time::Duration::days(1)), Some(5)).validate(start),
            Err("a recurrence may end with either a date or a count, not both"),
        );
        assert_eq!(
            rule(None, Some(0)).validate(start),
            Err("a recurrence count must be at least 1"),
        );
        assert_eq!(
            rule(Some(start), None).validate(start),
            Err("a recurrence must include the first occurrence"),
        );
        assert_eq!(
            rule(Some(start + time::Duration::hours(1)), None).validate(start),
            Ok(())
        );
        assert_eq!(rule(None, Some(1)).validate(start), Ok(()));
        assert_eq!(rule(None, None).validate(start), Ok(()));
    }

    #[test]
    fn create_request_validates_the_recurrence() {
        let req = CreateMeetingRequest {
            title: "Standup".into(),
            starts_at: OffsetDateTime::UNIX_EPOCH,
            ends_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            passcode: None,
            repeat: Some(RecurrenceRule {
                frequency: RepeatFrequency::Weekly,
                until: None,
                count: Some(0),
            }),
        };
        assert_eq!(req.validate(), Err("a recurrence count must be at least 1"));
    }

    #[test]
    fn update_request_distinguishes_absent_null_and_value() {
        // Absent: leave the recurrence unchanged.
        let req: UpdateMeetingRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.repeat, None);
        // JSON null: clear it back to a one-off.
        let req: UpdateMeetingRequest = serde_json::from_str(r#"{"repeat": null}"#).unwrap();
        assert_eq!(req.repeat, Some(None));
        // A value: replace the rule.
        let req: UpdateMeetingRequest =
            serde_json::from_str(r#"{"repeat": {"frequency": "monthly"}}"#).unwrap();
        assert_eq!(
            req.repeat,
            Some(Some(RecurrenceRule {
                frequency: RepeatFrequency::Monthly,
                until: None,
                count: None,
            }))
        );
    }

    #[test]
    fn create_request_validates_passcode() {
        let blank = CreateMeetingRequest {
            title: "Standup".into(),
            starts_at: OffsetDateTime::UNIX_EPOCH,
            ends_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            repeat: None,
            passcode: Some("   ".into()),
        };
        assert_eq!(blank.validate(), Err("passcode must not be empty"));

        let too_long = CreateMeetingRequest {
            passcode: Some("x".repeat(PASSCODE_MAX_LEN + 1)),
            ..blank
        };
        assert_eq!(
            too_long.validate(),
            Err("passcode must be at most 128 characters")
        );

        let fine = CreateMeetingRequest {
            passcode: Some("  sekrit  ".into()),
            ..too_long
        };
        assert_eq!(fine.validate(), Ok(()));
    }

    #[test]
    fn update_request_passcode_is_tri_state() {
        // Absent → `None`: leave the passcode unchanged.
        let unchanged: UpdateMeetingRequest = serde_json::from_str(r#"{"title": "New"}"#).unwrap();
        assert_eq!(unchanged.passcode, None);

        // Explicit null → `Some(None)`: remove the passcode.
        let removed: UpdateMeetingRequest = serde_json::from_str(r#"{"passcode": null}"#).unwrap();
        assert_eq!(removed.passcode, Some(None));

        // A value → `Some(Some(..))`: set the passcode.
        let set: UpdateMeetingRequest = serde_json::from_str(r#"{"passcode": "sekrit"}"#).unwrap();
        assert_eq!(set.passcode, Some(Some("sekrit".into())));

        // And each state serializes back to its wire shape.
        assert_eq!(serde_json::to_value(&set).unwrap()["passcode"], "sekrit");
        assert_eq!(
            serde_json::to_value(&removed).unwrap()["passcode"],
            serde_json::Value::Null
        );
        assert!(
            serde_json::to_value(&unchanged)
                .unwrap()
                .get("passcode")
                .is_none()
        );
    }

    #[test]
    fn meeting_response_defaults_upcoming_occurrences() {
        // Old payloads (before issue 16) deserialize with no recurrence:
        // `repeat` and `upcoming_occurrences` both default.
        let payload = r#"{
            "meeting": {
                "id": "00000000-0000-0000-0000-000000000000",
                "code": "ABCD-EFGH-JKLM",
                "title": "Standup",
                "starts_at": "2026-01-01T09:00:00Z",
                "ends_at": "2026-01-01T10:00:00Z",
                "cancelled": false
            },
            "joinability": {"type": "open"}
        }"#;
        let parsed: MeetingResponse = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.meeting.repeat, None);
        assert!(parsed.upcoming_occurrences.is_empty());
    }

    #[test]
    fn meeting_dto_has_passcode_flag_is_additive() {
        // Payloads that predate the flag still parse, as `false`.
        let legacy: MeetingDto = serde_json::from_str(
            r#"{
                "id": "00000000-0000-0000-0000-000000000000",
                "code": "H7KD-PM2X-QR4T",
                "title": "Standup",
                "starts_at": "1970-01-01T00:00:00Z",
                "ends_at": "1970-01-01T00:00:00Z",
                "cancelled": false
            }"#,
        )
        .unwrap();
        assert!(!legacy.has_passcode);

        let set = MeetingDto {
            has_passcode: true,
            ..legacy
        };
        assert_eq!(serde_json::to_value(&set).unwrap()["has_passcode"], true);
    }
}
