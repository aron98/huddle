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

/// A simulcast layer a receiver wants for a camera track. The focused tile
/// subscribes to the highest layer; everything else takes the low one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoLayer {
    Low,
    High,
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
    /// A recording is already running for this meeting.
    RecordingActive,
    /// The meeting has no live session to record (nobody has joined).
    MeetingNotLive,
    /// The join did not carry the meeting's passcode, or carried a wrong
    /// one. The client should prompt for a passcode and rejoin with it.
    PasscodeRequired,
    /// The meeting is end-to-end encrypted, so the requested action
    /// (recording) cannot run on it — ADR 010's interlock.
    E2eeActive,
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
        /// Per-meeting passcode, required when the meeting has one
        /// (issue #13). Compared in constant time server-side.
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
    /// Sent just before the screen-share track is added to the peer
    /// connection (and just after it is removed), so the SFU can tell the
    /// incoming screen m-line apart from the camera one.
    Sharing {
        sharing: bool,
    },
    /// Pick the simulcast layer to receive for another participant's camera
    /// track (focused tile → high, unfocused → low). Ignored for audio and
    /// single-layer screen shares.
    LayerPreference {
        origin: ParticipantId,
        layer: VideoLayer,
    },
    /// Hint: this receiver's focused (stage) participant, or none to
    /// clear it. The SFU arbitrates every camera's simulcast layer itself;
    /// focus only decides who wins the high layer when bandwidth cannot
    /// carry it for everyone (ADR 015, D5).
    Focus {
        origin: Option<ParticipantId>,
    },
    /// Opaque E2EE keying envelope, relayed to one other participant
    /// (issue #27 / ADR 010). `payload` is base64 of bytes the server
    /// must not interpret — identity announces, sender keys, and
    /// verification marks all ride this pipe. `epoch` is the meeting
    /// epoch the envelope belongs to; the server relays it untouched.
    E2eeKeying {
        to: ParticipantId,
        epoch: u64,
        payload: String,
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
    /// Answer to `Join`: who you are, where you landed, who else is there,
    /// and whether the meeting is currently being recorded (consent).
    Welcome {
        protocol_version: u32,
        you: Participant,
        meeting: crate::api::MeetingDto,
        roster: Vec<Participant>,
        ice_servers: Vec<IceServer>,
        /// `#[serde(default)]` keeps this additive for peers that predate
        /// recording support.
        #[serde(default)]
        recording: bool,
        /// Whether this meeting is end-to-end encrypted (ADR 010). Like
        /// `recording`, `#[serde(default)]` keeps the addition wire-safe.
        #[serde(default)]
        e2ee_enabled: bool,
        /// The meeting's current E2EE epoch: bumped by the server on
        /// every join/leave, identical on all sides by construction.
        /// Meaningful only when `e2ee_enabled` is true.
        #[serde(default)]
        e2ee_epoch: u64,
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
    /// Broadcast screen-share state, including echo back to the
    /// originator. Drives the "X is sharing" indicator and tile cleanup
    /// when a share stops.
    Sharing {
        id: ParticipantId,
        sharing: bool,
    },
    /// Broadcast when the organizer starts or stops a recording, so every
    /// participant knows the meeting is being recorded (consent indicator).
    Recording {
        active: bool,
    },
    /// Broadcast when the meeting's E2EE epoch advances — every join
    /// and leave bumps it. Participants provision fresh per-epoch sender
    /// keys on receipt (ADR 010). E2EE meetings only.
    E2eeEpoch {
        epoch: u64,
    },
    /// Relayed E2EE keying envelope, addressed to one participant.
    /// `payload` is opaque to the server (see [`ClientMessage::E2eeKeying`]).
    E2eeKeying {
        from: ParticipantId,
        to: ParticipantId,
        epoch: u64,
        payload: String,
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
                repeat: None,
                has_passcode: false,
                e2ee: false,
                title_ciphertext: None,
            },
            roster: vec![],
            ice_servers: vec![],
            recording: false,
            e2ee_enabled: false,
            e2ee_epoch: 0,
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
    fn layer_preference_round_trips() {
        let msg = ClientMessage::LayerPreference {
            origin: Uuid::nil(),
            layer: VideoLayer::High,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "layer_preference",
                "origin": Uuid::nil().to_string(),
                "layer": "high",
            })
        );
        assert_eq!(serde_json::from_value::<ClientMessage>(v).unwrap(), msg);

        let msg = ClientMessage::LayerPreference {
            origin: Uuid::nil(),
            layer: VideoLayer::Low,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["layer"], "low");
        assert_eq!(serde_json::from_value::<ClientMessage>(v).unwrap(), msg);
    }

    #[test]
    fn focus_round_trips() {
        let msg = ClientMessage::Focus {
            origin: Some(Uuid::nil()),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            v,
            json!({"type": "focus", "origin": Uuid::nil().to_string()})
        );
        assert_eq!(serde_json::from_value::<ClientMessage>(v).unwrap(), msg);

        // Clearing the focus is an explicit null.
        let msg = ClientMessage::Focus { origin: None };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v, json!({"type": "focus", "origin": null}));
        assert_eq!(serde_json::from_value::<ClientMessage>(v).unwrap(), msg);
    }

    #[test]
    fn recording_round_trips_and_defaults() {
        let msg = ServerMessage::Recording { active: true };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v, json!({"type": "recording", "active": true}));
        assert_eq!(serde_json::from_value::<ServerMessage>(v).unwrap(), msg);

        // The `recording` flag on `Welcome` is additive: old payloads
        // (without it) still parse, as false.
        let legacy = json!({
            "type": "welcome",
            "protocol_version": 1,
            "you": {
                "id": Uuid::nil().to_string(),
                "name": "alice",
                "audio_muted": false,
                "video_muted": false,
                "sharing": false,
            },
            "meeting": {
                "id": Uuid::nil().to_string(),
                "code": "H7KD-PM2X-QR4T",
                "title": "Standup",
                "starts_at": "1970-01-01T00:00:00Z",
                "ends_at": "1970-01-01T00:00:00Z",
                "cancelled": false,
            },
            "roster": [],
            "ice_servers": [],
        });
        let back: ServerMessage = serde_json::from_value(legacy).unwrap();
        let ServerMessage::Welcome {
            recording,
            e2ee_enabled,
            e2ee_epoch,
            meeting,
            ..
        } = back
        else {
            panic!("expected welcome");
        };
        assert!(!recording);
        // The E2EE fields are additive the same way: legacy payloads
        // parse as a plaintext meeting at epoch 0.
        assert!(!e2ee_enabled);
        assert_eq!(e2ee_epoch, 0);
        // Same additive story for the meeting's passcode flag.
        assert!(!meeting.has_passcode);
    }

    #[test]
    fn passcode_required_error_is_snake_case() {
        let msg = ServerMessage::Error {
            code: ErrorCode::PasscodeRequired,
            message: "this meeting requires a passcode".into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["code"], "passcode_required");
        assert_eq!(serde_json::from_value::<ServerMessage>(v).unwrap(), msg);
    }

    #[test]
    fn e2ee_keying_round_trips_both_directions() {
        let msg = ClientMessage::E2eeKeying {
            to: Uuid::nil(),
            epoch: 3,
            payload: "c2VjcmV0".into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "e2ee_keying",
                "to": Uuid::nil().to_string(),
                "epoch": 3,
                "payload": "c2VjcmV0",
            })
        );
        assert_eq!(serde_json::from_value::<ClientMessage>(v).unwrap(), msg);

        let msg = ServerMessage::E2eeKeying {
            from: Uuid::nil(),
            to: Uuid::max(),
            epoch: 3,
            payload: "c2VjcmV0".into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "e2ee_keying",
                "from": Uuid::nil().to_string(),
                "to": Uuid::max().to_string(),
                "epoch": 3,
                "payload": "c2VjcmV0",
            })
        );
        assert_eq!(serde_json::from_value::<ServerMessage>(v).unwrap(), msg);
    }

    #[test]
    fn e2ee_epoch_broadcast_round_trips() {
        let msg = ServerMessage::E2eeEpoch { epoch: 7 };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v, json!({"type": "e2ee_epoch", "epoch": 7}));
        assert_eq!(serde_json::from_value::<ServerMessage>(v).unwrap(), msg);
    }

    #[test]
    fn e2ee_active_error_is_snake_case() {
        let msg = ServerMessage::Error {
            code: ErrorCode::E2eeActive,
            message: "this meeting is end-to-end encrypted".into(),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["code"], "e2ee_active");
        assert_eq!(serde_json::from_value::<ServerMessage>(v).unwrap(), msg);
    }

    #[test]
    fn client_join_carries_token_when_present() {
        let msg = ClientMessage::Join {
            meeting_code: "H7KD-PM2X-QR4T".into(),
            name: "alice".into(),
            token: Some("sekrit".into()),
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["token"], "sekrit");
        assert_eq!(serde_json::from_value::<ClientMessage>(v).unwrap(), msg);
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

    #[test]
    fn sharing_round_trips_both_directions() {
        let msg = ClientMessage::Sharing { sharing: true };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v, json!({"type": "sharing", "sharing": true}));
        assert_eq!(serde_json::from_value::<ClientMessage>(v).unwrap(), msg);

        let msg = ServerMessage::Sharing {
            id: Uuid::nil(),
            sharing: false,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            v,
            json!({"type": "sharing", "id": Uuid::nil().to_string(), "sharing": false})
        );
        assert_eq!(serde_json::from_value::<ServerMessage>(v).unwrap(), msg);
    }
}
