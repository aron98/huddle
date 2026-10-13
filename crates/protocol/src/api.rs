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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MeetingResponse {
    pub meeting: MeetingDto,
    pub joinability: Joinability,
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
        Ok(())
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
        };
        assert_eq!(req.validate(), Err("title must not be empty"));
    }

    #[test]
    fn create_request_rejects_backwards_times() {
        let req = CreateMeetingRequest {
            title: "Standup".into(),
            starts_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(2),
            ends_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        };
        assert_eq!(req.validate(), Err("ends_at must be after starts_at"));
    }
}
