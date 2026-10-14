//! Signaling messages carried over the WebSocket.
//!
//! Wire format: JSON with a `"type"` tag (lowercase snake_case), chosen for
//! MVP debuggability; swappable for MessagePack later without changing
//! message semantics (see `docs/ARCHITECTURE.md` → Signaling protocol).

use time::OffsetDateTime;
use uuid::Uuid;

/// A participant identity inside a meeting.
pub type ParticipantId = Uuid;

/// What a media track carries. Screen shares get different SFU treatment
/// (no simulcast, generous bitrate) than camera tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Audio,
    Camera,
    Screen,
}

/// A participant as shown in the roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub id: ParticipantId,
    pub name: String,
    pub audio_muted: bool,
    pub video_muted: bool,
    pub sharing: bool,
}

/// ICE/TURN servers the client should use, advertised by the server on join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// A chat message relayed within a meeting. `sender_id: None` marks a
/// system message ("Alice joined").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<ParticipantId>,
    pub sender_name: String,
    pub body: String,
    #[serde(with = "time::serde::rfc3339")]
    pub sent_at: OffsetDateTime,
}

/// Machine-readable error codes. Clients switch on these; `message` is for
/// humans and may change between releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    MeetingNotFound,
    MeetingNotYetOpen,
    MeetingEnded,
    MeetingCancelled,
    CapacityReached,
    NameTaken,
    Unauthorized,
    InvalidMessage,
    Internal,
}

/// Client → server messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "snake_case"
)]
pub enum ClientMessage {
    /// First message on the socket; everything else is refused until it lands.
    Join {
        meeting_code: String,
        name: String,
        /// Reserved for per-meeting passcodes (post-MVP).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// SDP offer from the browser's peer connection, in SDP text form.
    SdpOffer {
        sdp: String,
    },
    /// SDP answer to a server-initiated offer (renegotiation when the SFU
    /// needs to add or remove forwarded tracks).
    SdpAnswer {
        sdp: String,
    },
    /// Trickled ICE candidate from the browser, candidate string form.
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_m_line_index: Option<u16>,
    },
    Chat {
        body: String,
    },
    /// Self-reported mute/camera state changes.
    MuteState {
        audio_muted: Option<bool>,
        video_muted: Option<bool>,
    },
    /// Self-reported voice activity (detected locally, relayed so every
    /// participant can ring the speaker's tile).
    Speaking {
        speaking: bool,
    },
    /// Graceful leave; the socket closing implies the same.
    Leave,
}

/// Server → client messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "snake_case"
)]
pub enum ServerMessage {
    /// Answer to `Join`: who you are, where you landed, who else is there.
    Welcome {
        protocol_version: u32,
        you: Participant,
        meeting: crate::api::MeetingDto,
        roster: Vec<Participant>,
        ice_servers: Vec<IceServer>,
    },
    ParticipantJoined {
        participant: Participant,
    },
    ParticipantLeft {
        id: ParticipantId,
    },
    SdpAnswer {
        sdp: String,
    },
    /// Server-initiated offer: the SFU renegotiates to add/remove forwarded
    /// tracks. The client replies with [`ClientMessage::SdpAnswer`].
    SdpOffer {
        sdp: String,
    },
    /// Trickled ICE candidate from the server side.
    IceCandidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_m_line_index: Option<u16>,
    },
    Chat {
        message: ChatMessage,
    },
    /// Broadcast mute/camera state, including echo back to the originator.
    MuteState {
        id: ParticipantId,
        audio_muted: bool,
        video_muted: bool,
    },
    /// Broadcast voice activity, including echo back to the originator so
    /// every client has one authoritative speaker set.
    Speaking {
        id: ParticipantId,
        speaking: bool,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn client_join_json_shape_is_stable() {
        let msg = ClientMessage::Join {
            meeting_code: "H7KD-PM2X-QR4T".into(),
            name: "alice".into(),
            token: None,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            v,
            json!({"type": "join", "meeting_code": "H7KD-PM2X-QR4T", "name": "alice"})
        );
        let back: ClientMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn client_ice_candidate_round_trips() {
        let msg = ClientMessage::IceCandidate {
            candidate: "candidate:1 1 udp 2130706431 192.168.1.2 54321 typ host".into(),
            sdp_mid: Some("0".into()),
            sdp_m_line_index: Some(0),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "ice_candidate");
        assert_eq!(v["sdp_mid"], "0");
        let back: ClientMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn welcome_json_shape_is_stable() {
        let msg = ServerMessage::Welcome {
            protocol_version: 1,
            you: Participant {
                id: Uuid::nil(),
                name: "alice".into(),
                audio_muted: false,
                video_muted: false,
                sharing: false,
            },
            meeting: crate::api::MeetingDto {
                id: Uuid::nil(),
                code: "H7KD-PM2X-QR4T".into(),
                title: "Standup".into(),
                starts_at: OffsetDateTime::UNIX_EPOCH,
                ends_at: OffsetDateTime::UNIX_EPOCH,
                cancelled: false,
            },
            roster: vec![],
            ice_servers: vec![],
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "welcome");
        assert_eq!(v["you"]["name"], "alice");
        assert_eq!(v["protocol_version"], 1);
        let back: ServerMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn chat_message_round_trips_rfc3339() {
        let msg = ServerMessage::Chat {
            message: ChatMessage {
                id: Uuid::nil(),
                sender_id: None,
                sender_name: "system".into(),
                body: "alice joined".into(),
                sent_at: OffsetDateTime::UNIX_EPOCH,
            },
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert!(v["message"]["sent_at"].as_str().unwrap().ends_with("Z"));
        let back: ServerMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn error_codes_are_snake_case() {
        let msg = ServerMessage::Error {
            code: ErrorCode::MeetingNotYetOpen,
            message: "opens in 10 minutes".into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["code"], "meeting_not_yet_open");
    }

    #[test]
    fn client_sdp_answer_round_trips() {
        let msg = ClientMessage::SdpAnswer {
            sdp: "v=0 answer".into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v, json!({"type": "sdp_answer", "sdp": "v=0 answer"}));
        let back: ClientMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn server_sdp_offer_round_trips() {
        let msg = ServerMessage::SdpOffer {
            sdp: "v=0 offer".into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v, json!({"type": "sdp_offer", "sdp": "v=0 offer"}));
        let back: ServerMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn unknown_type_is_rejected() {
        let err = serde_json::from_str::<ClientMessage>(r#"{"type": "nope"}"#);
        assert!(err.is_err());
    }

    #[test]
    fn speaking_round_trips_both_directions() {
        let msg = ClientMessage::Speaking { speaking: true };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v, json!({"type": "speaking", "speaking": true}));
        assert_eq!(serde_json::from_value::<ClientMessage>(v).unwrap(), msg);

        let msg = ServerMessage::Speaking {
            id: Uuid::nil(),
            speaking: false,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            v,
            json!({"type": "speaking", "id": Uuid::nil().to_string(), "speaking": false})
        );
        assert_eq!(serde_json::from_value::<ServerMessage>(v).unwrap(), msg);
    }
}
