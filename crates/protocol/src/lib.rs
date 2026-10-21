#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The huddle wire protocol.
//!
//! Every message that crosses the signaling WebSocket or the REST API is
//! defined here. This crate is compiled into both the server and the WASM
//! client, so interface drift is a compile error on one side or the other
//! (see `docs/ARCHITECTURE.md`).

pub mod api;
pub mod signaling;

/// Bumped on breaking wire-format changes. Sent in `Welcome` so clients can
/// refuse gracefully instead of misbehaving.
pub const PROTOCOL_VERSION: u32 = 1;

pub use api::{
    ApiError, CreateMeetingRequest, ENCRYPTED_TITLE_MAX_LEN, ENCRYPTED_TITLE_PREFIX, Joinability,
    MASKED_TITLE, MeetingDto, MeetingResponse, OccurrenceDto, PASSCODE_MAX_LEN, RecordingDto,
    RecurrenceRule, RepeatFrequency, TITLE_MAX_LEN, UpdateMeetingRequest, display_title,
    is_encrypted_title, validate_passcode, validate_title,
};
pub use signaling::{
    ChatMessage, ClientMessage, ErrorCode, IceServer, Participant, ParticipantId, ServerMessage,
    TrackKind, VideoLayer,
};
