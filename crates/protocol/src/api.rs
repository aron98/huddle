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
    /// Whether the meeting is end-to-end encrypted (issue #27): media
    /// keys live only on the participants' clients and the server relays
    /// ciphertext. Set at creation; not changeable afterwards.
    #[serde(default)]
    pub e2ee: bool,
    /// The client-side encrypted title blob (issue #29) for E2EE
    /// meetings: `"<ENCRYPTED_TITLE_PREFIX>" + base64(nonce ‖ AES-GCM
    /// ciphertext)`, sealed by the creating client to the organizer's
    /// identity key. Present only when the stored title is encrypted;
    /// in that case [`MeetingDto::title`] carries the masked label and
    /// clients that can decrypt replace it locally. `None` for plaintext
    /// meetings and for old servers — additive, so legacy payloads
    /// parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_ciphertext: Option<String>,
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
    /// Create the meeting in end-to-end encrypted mode (issue #27): the
    /// clients exchange media keys pairwise and the server only ever
    /// relays ciphertext — which also means the meeting cannot be
    /// recorded (ADR 010's interlock). Absent means a plaintext meeting.
    #[serde(default)]
    pub e2ee: bool,
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
        validate_title(&self.title)?;
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

/// Maximum plaintext title length, shared by create and update
/// validation. The client enforces this *before* encrypting.
pub const TITLE_MAX_LEN: usize = 200;

/// Marker prefix of a client-side encrypted title in the `title`
/// column (issue #29). The stored string is self-describing: servers
/// and old clients can tell an encrypted title from a plaintext one
/// without a schema change or an extra flag.
pub const ENCRYPTED_TITLE_PREFIX: &str = "enc1.";

/// Upper bound for an encrypted title blob (prefix + base64 of a
/// 12-byte nonce, the ciphertext, and a 16-byte tag). Ample headroom
/// over the 200-character plaintext cap.
pub const ENCRYPTED_TITLE_MAX_LEN: usize = 2048;

/// What the server reports as a meeting's title when the stored title
/// is encrypted and cannot be decrypted for the viewer: UIs and the
/// ICS export show this instead of ever rendering ciphertext.
pub const MASKED_TITLE: &str = "Encrypted meeting";

/// Whether a stored title string is a client-side encrypted blob
/// rather than plaintext.
pub fn is_encrypted_title(title: &str) -> bool {
    title.starts_with(ENCRYPTED_TITLE_PREFIX)
}

/// The title to display for a stored title string: the masked label
/// for encrypted blobs (only the organizer's own client can decrypt
/// those), the title itself for plaintext meetings.
pub fn display_title(title: &str) -> &str {
    if is_encrypted_title(title) {
        MASKED_TITLE
    } else {
        title
    }
}

/// Validate a title, plaintext or encrypted (issue #29): plaintext
/// titles must be trimmed, non-empty and at most [`TITLE_MAX_LEN`]
/// characters; encrypted blobs must fit [`ENCRYPTED_TITLE_MAX_LEN`]
/// (the plaintext inside was validated by the client *before*
/// sealing, so the server only bounds the stored size). Shared by
/// the create request and the server's update path.
pub fn validate_title(title: &str) -> Result<(), &'static str> {
    if is_encrypted_title(title) {
        if title.len() > ENCRYPTED_TITLE_MAX_LEN {
            return Err("encrypted title must be at most 2048 characters");
        }
        return Ok(());
    }
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err("title must not be empty");
    }
    if trimmed.len() > TITLE_MAX_LEN {
        return Err("title must be at most 200 characters");
    }
    Ok(())
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

// --- OAuth authorization server (issue #9, ADR 013) ------------------------
//
// huddle acts as its own OAuth authorization server for machine
// clients (CLIs, agent hosts): the device-authorization flow of
// RFC 8628 issues scoped, revocable tokens to the REST API below.
// These are the wire shapes of that surface, shared by the server and
// the CLI exactly like the meeting DTOs above.

/// One capability an access token may carry. Wire format is the RFC
/// 6749 `scope` token syntax: `meetings:read`, `meetings:write`.
///
/// Scopes are orthogonal on purpose: `meetings:write` does NOT imply
/// `meetings:read` — a write-only token can create and cancel but
/// cannot list. Clients that want both ask for both (the CLI does).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    #[serde(rename = "meetings:read")]
    MeetingsRead,
    #[serde(rename = "meetings:write")]
    MeetingsWrite,
}

impl Scope {
    /// Every scope the server can issue today. Additive over time.
    pub const ALL: [Scope; 2] = [Scope::MeetingsRead, Scope::MeetingsWrite];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::MeetingsRead => "meetings:read",
            Scope::MeetingsWrite => "meetings:write",
        }
    }

    /// Parse one scope token; `None` for anything unknown.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "meetings:read" => Some(Scope::MeetingsRead),
            "meetings:write" => Some(Scope::MeetingsWrite),
            _ => None,
        }
    }

    /// Parse a space-separated RFC 6749 `scope` value. `None` when any
    /// token is unknown or the list is empty.
    pub fn parse_list(raw: &str) -> Option<Vec<Scope>> {
        let scopes: Vec<Scope> = raw
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(Scope::parse)
            .collect::<Option<Vec<_>>>()?;
        (!scopes.is_empty()).then_some(scopes)
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Render scopes as an RFC 6749 `scope` value (space-separated).
pub fn scopes_string(scopes: &[Scope]) -> String {
    scopes
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `grant_type` of RFC 8628's token request.
pub const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Success body of `POST /api/oauth/device/register` (RFC 8628 §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAuthorizationResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// `verification_uri` with the code pre-filled, so an
    /// already-signed-in browser can approve with one visit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    /// Seconds until the device code expires.
    pub expires_in: u32,
    /// Minimum seconds between token polls.
    pub interval: u32,
}

/// Success body of `POST /api/oauth/token` (RFC 6749 §5.1 shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    /// Always `Bearer`.
    pub token_type: String,
    pub expires_in: u32,
    /// Present for the device grant (and absent on pure refreshes,
    /// which keep the same refresh token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// The scopes the access token carries, space-separated.
    pub scope: String,
}

/// Error body of the OAuth endpoints (RFC 6749 §5.2 shape):
/// `{"error": "authorization_pending", ...}`. Distinct from
/// [`ApiError`] on purpose — OAuth clients speak RFC error codes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

/// One active client authorization on the organizer's dashboard: the
/// long-lived credential a device holds, with the human-facing label
/// (client name) and the scopes it was granted. The token value itself
/// is never returned — only its server-side identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationDto {
    pub id: Uuid,
    pub client_name: String,
    pub scopes: Vec<Scope>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
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
    fn scopes_round_trip_through_their_rfc_names() {
        for scope in Scope::ALL {
            let json = serde_json::to_string(&scope).unwrap();
            assert_eq!(json, format!("\"{}\"", scope.as_str()));
            let back: Scope = serde_json::from_str(&json).unwrap();
            assert_eq!(back, scope);
            assert_eq!(Scope::parse(scope.as_str()), Some(scope));
        }
        assert_eq!(scopes_string(&Scope::ALL), "meetings:read meetings:write");
    }

    #[test]
    fn scope_lists_parse_and_reject_garbage() {
        let scopes = Scope::parse_list("meetings:read  meetings:write").unwrap();
        assert_eq!(scopes, Scope::ALL);
        // Empty, unknown, and half-known lists are all refused — a
        // client asking for a scope we do not issue must fail loudly.
        assert!(Scope::parse_list("").is_none());
        assert!(Scope::parse_list("meetings:admin").is_none());
        assert!(Scope::parse_list("meetings:read meetings:admin").is_none());
    }

    #[test]
    fn oauth_wire_shapes_match_the_rfc() {
        let device = serde_json::json!({
            "device_code": "dc", "user_code": "ABCD-EFGH",
            "verification_uri": "https://x/oauth/device",
            "expires_in": 600, "interval": 5,
        });
        let parsed: DeviceAuthorizationResponse = serde_json::from_value(device).unwrap();
        assert_eq!(parsed.verification_uri_complete, None);

        let token = OAuthTokenResponse {
            access_token: "at".into(),
            token_type: "Bearer".into(),
            expires_in: 1800,
            refresh_token: Some("rt".into()),
            scope: "meetings:read meetings:write".into(),
        };
        let json = serde_json::to_string(&token).unwrap();
        assert!(json.contains("\"refresh_token\":\"rt\""));
        let back: OAuthTokenResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back, token);

        // The RFC 6749 error body is flat, not the ApiError envelope.
        let err = serde_json::to_value(OAuthError {
            error: "authorization_pending".into(),
            error_description: None,
        })
        .unwrap();
        assert_eq!(err["error"], "authorization_pending");
        assert!(err.get("error_description").is_none());
    }

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
            e2ee: false,
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
            e2ee: false,
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
            e2ee: false,
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
            e2ee: false,
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

    #[test]
    fn e2ee_flags_are_additive() {
        // Payloads that predate E2EE still parse, as plaintext meetings.
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
        assert!(!legacy.e2ee);

        // And a create request defaults to a plaintext meeting.
        let req: CreateMeetingRequest =
            serde_json::from_str(r#"{"title": "S", "starts_at": "1970-01-01T00:00:00Z", "ends_at": "1970-01-01T00:00:00Z"}"#)
                .unwrap();
        assert!(!req.e2ee);

        // While an E2EE meeting round-trips the flag.
        let encrypted = MeetingDto {
            e2ee: true,
            ..legacy
        };
        assert_eq!(serde_json::to_value(&encrypted).unwrap()["e2ee"], true);
        let req = CreateMeetingRequest { e2ee: true, ..req };
        assert_eq!(serde_json::to_value(&req).unwrap()["e2ee"], true);
    }

    #[test]
    fn title_ciphertext_is_additive() {
        // Payloads that predate encrypted titles still parse: no blob.
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
        assert_eq!(legacy.title_ciphertext, None);
        assert!(
            serde_json::to_value(&legacy)
                .unwrap()
                .get("title_ciphertext")
                .is_none(),
            "the field is skipped, not null"
        );

        // An encrypted meeting carries the blob alongside the masked title.
        let encrypted = MeetingDto {
            title: MASKED_TITLE.into(),
            title_ciphertext: Some("enc1.AQIDBAU=".into()),
            ..legacy
        };
        let value = serde_json::to_value(&encrypted).unwrap();
        assert_eq!(value["title"], MASKED_TITLE);
        assert_eq!(value["title_ciphertext"], "enc1.AQIDBAU=");
    }

    #[test]
    fn encrypted_titles_validate_differently_from_plaintext() {
        // Encrypted blobs skip the plaintext shape checks and only bound
        // the stored size; whitespace is not trimmed (base64 has none).
        assert_eq!(validate_title("enc1.AQIDBAU="), Ok(()));
        assert_eq!(
            validate_title(&format!("enc1.{}", "A".repeat(ENCRYPTED_TITLE_MAX_LEN))),
            Err("encrypted title must be at most 2048 characters"),
        );

        // Plaintext titles keep their existing rules, shared by create and
        // update paths.
        assert_eq!(validate_title("  "), Err("title must not be empty"));
        assert_eq!(
            validate_title(&"x".repeat(TITLE_MAX_LEN + 1)),
            Err("title must be at most 200 characters"),
        );
        assert_eq!(validate_title(" Standup "), Ok(()));

        // A create request with an encrypted title passes title validation
        // (the other field checks still apply).
        let req = CreateMeetingRequest {
            title: "enc1.AQIDBAU=".into(),
            starts_at: OffsetDateTime::UNIX_EPOCH,
            ends_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            repeat: None,
            passcode: None,
            e2ee: true,
        };
        assert_eq!(req.validate(), Ok(()));
    }

    #[test]
    fn display_title_masks_only_encrypted_titles() {
        assert_eq!(display_title("Standup"), "Standup");
        assert_eq!(display_title(MASKED_TITLE), MASKED_TITLE);
        assert_eq!(display_title("enc1.AQIDBAU="), MASKED_TITLE);
        assert!(is_encrypted_title("enc1."));
        assert!(!is_encrypted_title("Standup"));
        assert!(!is_encrypted_title(""));
        assert!(!is_encrypted_title("enc2.AQIDBAU="));
    }
}
