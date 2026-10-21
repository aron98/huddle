//! The user-flow state machine. See `docs/user-flow.md` for the full
//! diagram; this module is its executable form.
//!
//! ```text
//! Prejoin ──join──> Joining ──Welcome+publish──> InCall
//!    ^                 │                           │        │
//!    │                 │ (every refusal,            │        │
//!    └─────────────────┘  see JoinError)            │        │
//!                                        user Leave │        │ drop
//!                                                 v         v
//!                                               Left    ConnectionLost
//!                     (rejoin/reconnect both return to Prejoin via reload)
//! ```

use crate::rtc::RtcError;
use huddle_protocol::ErrorCode;

/// Where a meeting-goer is right now. `MeetingFlow` (in `crates/web/src/call.rs`)
/// owns the current phase and renders the matching view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlowPhase {
    /// On `/m/{code}`: name, passcode when required, camera preview,
    /// device pickers.
    Prejoin,
    /// Join clicked: WS connect, join, and publish are in flight.
    Joining,
    /// Media is flowing: tiles, roster, chat.
    InCall,
    /// The call is over — and how it ended decides the view.
    Ended(LeaveKind),
}

/// How a call ended. User-initiated leave and connection loss are
/// deliberately distinct views, not one "you're out" page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaveKind {
    /// The user clicked "Leave".
    YouLeft,
    /// The connection dropped or a send failed mid-call; carries the
    /// human-oriented reason.
    ConnectionLost(String),
}

/// Every way the join handshake can fail, mapped one-to-one from the
/// server's join paths (`session.rs::expect_join` / `try_join`) and the
/// local transport. Each variant has distinct, actionable copy — see
/// [`JoinError::headline`] and [`JoinError::detail`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinError {
    /// The WebSocket could not be opened at all.
    Unreachable,
    /// The browser could not create an `RTCPeerConnection`.
    PeerSetup(String),
    /// No meeting exists under this link.
    NotFound,
    /// The meeting exists but its start time is in the future; carries the
    /// locally formatted opening time when the prejoin fetch knows it.
    NotOpenYet { opens_at: Option<String> },
    /// The meeting's end time has passed.
    Ended,
    /// The meeting was cancelled by its organizer.
    Cancelled,
    /// Someone in the room already uses this name — an inline field error.
    NameTaken,
    /// The meeting is protected by a passcode that was missing or wrong —
    /// an inline field error, like [`JoinError::NameTaken`].
    PasscodeRequired,
    /// The meeting is at capacity.
    Full,
    /// The server refused the join for any other reason; carries its message.
    Server(String),
    /// The `Welcome` announced a protocol the client cannot speak.
    ProtocolVersion(u32),
}

impl JoinError {
    /// Map a failed join to its distinct UI state. `opens_at` is the
    /// locally formatted opening time from the prejoin meeting fetch, used
    /// to make "not open yet" actionable.
    pub fn from_join_failure(err: &RtcError, opens_at: Option<String>) -> Self {
        match err {
            RtcError::Server(server) => match server.code {
                ErrorCode::MeetingNotFound => JoinError::NotFound,
                ErrorCode::MeetingNotYetOpen => JoinError::NotOpenYet { opens_at },
                ErrorCode::MeetingEnded => JoinError::Ended,
                ErrorCode::MeetingCancelled => JoinError::Cancelled,
                ErrorCode::NameTaken => JoinError::NameTaken,
                ErrorCode::CapacityReached => JoinError::Full,
                ErrorCode::PasscodeRequired => JoinError::PasscodeRequired,
                ErrorCode::InvalidMessage
                | ErrorCode::Internal
                | ErrorCode::Unauthorized
                | ErrorCode::RecordingActive
                | ErrorCode::MeetingNotLive
                | ErrorCode::E2eeActive => JoinError::Server(server.message.clone()),
            },
            // A transport error before the Welcome means the handshake
            // never completed: the server is unreachable from here.
            RtcError::Signaling(_) => JoinError::Unreachable,
            RtcError::Peer(_) => JoinError::Unreachable,
            RtcError::ProtocolVersion(v) => JoinError::ProtocolVersion(*v),
            // NotJoined/Closed cannot surface from `join` itself.
            RtcError::NotJoined | RtcError::Closed => {
                JoinError::Server("the session closed before the join finished".to_owned())
            }
        }
    }

    /// The one-line title for the error UI.
    pub fn headline(&self) -> &'static str {
        match self {
            JoinError::Unreachable => "Can't reach the server",
            JoinError::PeerSetup(_) => "WebRTC is unavailable",
            JoinError::NotFound => "This link doesn't match a meeting",
            JoinError::NotOpenYet { .. } => "The meeting hasn't started yet",
            JoinError::Ended => "This meeting has ended",
            JoinError::Cancelled => "This meeting was cancelled",
            JoinError::NameTaken => "That name is taken",
            JoinError::PasscodeRequired => "This meeting needs a passcode",
            JoinError::Full => "This meeting is full",
            JoinError::Server(_) => "Couldn't join",
            JoinError::ProtocolVersion(_) => "Update huddle to join",
        }
    }

    /// The actionable body: what went wrong and what to do about it.
    pub fn detail(&self, name: &str) -> String {
        match self {
            JoinError::Unreachable => "Check that the huddle server is running and your \
                network allows the connection, then try again."
                .to_owned(),
            JoinError::PeerSetup(detail) => {
                format!(
                    "This browser could not set up a call ({detail}). Try a recent Chrome or Firefox."
                )
            }
            JoinError::NotFound => {
                "The code in this link doesn't belong to any meeting. Ask the organizer \
                to send a fresh link."
                    .to_owned()
            }
            JoinError::NotOpenYet { opens_at } => match opens_at {
                Some(when) => format!(
                    "It opens at {when}. Your camera is already set up — join again \
                    once it's open."
                ),
                None => {
                    "It hasn't reached its start time yet. Join again once it opens.".to_owned()
                }
            },
            JoinError::Ended => {
                "Its end time has passed. Ask the organizer to schedule a new one, \
                or check your meetings page."
                    .to_owned()
            }
            JoinError::Cancelled => {
                "The organizer cancelled this meeting. Ask them to schedule a new one.".to_owned()
            }
            JoinError::NameTaken => {
                format!("Someone is already called \"{name}\" in this meeting. Try your full name.")
            }
            JoinError::PasscodeRequired => {
                "The passcode is missing or wrong. Enter the meeting's passcode — ask the \
                organizer for it if you don't have it — then join again."
                    .to_owned()
            }
            JoinError::Full => "Everyone who can join has joined. Ask the organizer to make room, \
                then try again."
                .to_owned(),
            JoinError::Server(detail) => {
                format!("The server refused the join: {detail}. Try again.")
            }
            JoinError::ProtocolVersion(v) => format!(
                "This server speaks protocol version {v}, which this client doesn't \
                understand yet. Update huddle and reload."
            ),
        }
    }
}
