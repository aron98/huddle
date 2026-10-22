//! Prometheus text-format rendering of the engine's stats snapshot.
//!
//! `/metrics` is a pull endpoint: every scrape asks the engine for a
//! [`StatsReport`] and renders it here. No metric library is involved —
//! the exposition format is deliberately simple, and per-room/per-peer
//! data (labels: `room`, `peer`, `id`) is exactly what it was designed
//! for. See <https://prometheus.io/docs/instrumenting/exposition_formats/>.

use std::fmt::Write as _;

use huddle_protocol::VideoLayer;

use crate::engine::{PeerReport, StatsReport};

/// Content type of the Prometheus text exposition format (version 0.0.4).
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Render the whole report in the Prometheus text exposition format.
pub fn render(report: &StatsReport) -> String {
    let mut out = String::new();

    header(
        &mut out,
        "huddle_rooms",
        "gauge",
        "Number of live meeting rooms.",
    );
    let _ = writeln!(out, "huddle_rooms {}", report.rooms.len());

    header(
        &mut out,
        "huddle_room_participants",
        "gauge",
        "Participants currently in each live room.",
    );
    for room in &report.rooms {
        let _ = writeln!(
            out,
            "huddle_room_participants{{room=\"{}\"}} {}",
            escape(&room.code),
            room.peers.len()
        );
    }

    peer_gauge(
        &mut out,
        report,
        "huddle_peer_bitrate_in_bps",
        "Ingress bitrate toward the SFU over the last stats window, bits per second.",
        |p| Some(p.bitrate_in_bps),
    );
    peer_gauge(
        &mut out,
        report,
        "huddle_peer_bitrate_out_bps",
        "Egress bitrate from the SFU over the last stats window, bits per second.",
        |p| Some(p.bitrate_out_bps),
    );
    peer_gauge(
        &mut out,
        report,
        "huddle_peer_rtt_ms",
        "Most recent round-trip time to this peer, milliseconds.",
        |p| p.rtt_ms,
    );
    peer_gauge_f32(
        &mut out,
        report,
        "huddle_peer_ingress_loss_fraction",
        "Packet loss on the wire from this peer (fraction of 1.0).",
        |p| p.ingress_loss_fraction,
    );
    peer_gauge_f32(
        &mut out,
        report,
        "huddle_peer_egress_loss_fraction",
        "Packet loss on the wire toward this peer (fraction of 1.0).",
        |p| p.egress_loss_fraction,
    );

    peer_counter(
        &mut out,
        report,
        "huddle_peer_rx_frames_total",
        "Media frames received from this peer.",
        |p| p.rx_frames,
    );
    peer_counter(
        &mut out,
        report,
        "huddle_peer_tx_frames_total",
        "Media frames sent to this peer.",
        |p| p.tx_frames,
    );
    peer_counter(
        &mut out,
        report,
        "huddle_peer_keyframes_requested_total",
        "Keyframe (FIR/PLI) requests enqueued toward this peer.",
        |p| p.keyframes_requested,
    );
    peer_counter(
        &mut out,
        report,
        "huddle_peer_rx_bytes_total",
        "Media payload bytes received from this peer.",
        |p| p.bytes_rx,
    );
    peer_counter(
        &mut out,
        report,
        "huddle_peer_tx_bytes_total",
        "Media payload bytes sent to this peer.",
        |p| p.bytes_tx,
    );

    header(
        &mut out,
        "huddle_peer_simulcast_layer",
        "gauge",
        "Simulcast layer this peer currently receives from the origin's camera (0 = low, 1 = high).",
    );
    for room in &report.rooms {
        for peer in &room.peers {
            for sub in &peer.simulcast_layers {
                let layer = match sub.layer {
                    VideoLayer::High => 1,
                    VideoLayer::Low => 0,
                };
                let _ = writeln!(
                    out,
                    "huddle_peer_simulcast_layer{{room=\"{}\",peer=\"{}\",id=\"{}\",origin=\"{}\"}} {layer}",
                    escape(&room.code),
                    escape(&peer.name),
                    peer.id,
                    escape(&sub.origin)
                );
            }
        }
    }

    out
}

fn header(out: &mut String, name: &str, type_: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {type_}");
}

/// Emit one counter family: header, then a sample per peer.
fn peer_counter(
    out: &mut String,
    report: &StatsReport,
    name: &str,
    help: &str,
    value: impl Fn(&PeerReport) -> u64,
) {
    header(out, name, "counter", help);
    for_each_peer(out, report, name, |p| Some(value(p).to_string()));
}

/// Emit one gauge family: header, then a sample per peer.
fn peer_gauge(
    out: &mut String,
    report: &StatsReport,
    name: &str,
    help: &str,
    value: impl Fn(&PeerReport) -> Option<u64>,
) {
    header(out, name, "gauge", help);
    for_each_peer(out, report, name, |p| value(p).map(|v| v.to_string()));
}

/// Like [`peer_gauge`] for `f32`-valued gauges (loss fractions).
fn peer_gauge_f32(
    out: &mut String,
    report: &StatsReport,
    name: &str,
    help: &str,
    value: impl Fn(&PeerReport) -> Option<f32>,
) {
    header(out, name, "gauge", help);
    for_each_peer(out, report, name, |p| value(p).map(|v| v.to_string()));
}

/// The shared sample loop: `None` values are skipped entirely (a missing
/// sample is the Prometheus idiom for "not known yet").
fn for_each_peer(
    out: &mut String,
    report: &StatsReport,
    name: &str,
    value: impl Fn(&PeerReport) -> Option<String>,
) {
    for room in &report.rooms {
        for peer in &room.peers {
            let Some(v) = value(peer) else { continue };
            let _ = writeln!(
                out,
                "{name}{{room=\"{}\",peer=\"{}\",id=\"{}\"}} {v}",
                escape(&room.code),
                escape(&peer.name),
                peer.id
            );
        }
    }
}

/// Escape a label value per the exposition format: backslash, double
/// quote, and newline (as `\\n`) must not appear literally.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::engine::{PeerReport, RoomReport, SimulcastLayer};
    use uuid::Uuid;

    fn sample_report(rtt_ms: Option<u64>, loss: Option<f32>) -> StatsReport {
        StatsReport {
            rooms: vec![RoomReport {
                code: "ABCD-EFGH-JKLM".into(),
                peers: vec![PeerReport {
                    id: Uuid::nil(),
                    name: "alice".into(),
                    rx_frames: 10,
                    tx_frames: 20,
                    keyframes_requested: 1,
                    bytes_rx: 400,
                    bytes_tx: 800,
                    bitrate_in_bps: 1600,
                    bitrate_out_bps: 3200,
                    rtt_ms,
                    ingress_loss_fraction: loss,
                    egress_loss_fraction: loss,
                    simulcast_layers: vec![SimulcastLayer {
                        origin: "bob".into(),
                        layer: VideoLayer::Low,
                    }],
                }],
            }],
        }
    }

    #[test]
    fn empty_report_renders_zero_rooms() {
        let out = render(&StatsReport::default());
        assert!(out.contains("# TYPE huddle_rooms gauge\n"));
        assert!(out.contains("huddle_rooms 0\n"));
        assert!(!out.contains("huddle_peer_rtt_ms{"), "no samples expected");
    }

    #[test]
    fn renders_typed_families_and_labeled_samples() {
        let out = render(&sample_report(Some(42), Some(0.01)));
        assert!(out.contains("# TYPE huddle_peer_rx_bytes_total counter\n"));
        assert!(out.contains(
            "huddle_peer_rx_bytes_total{room=\"ABCD-EFGH-JKLM\",peer=\"alice\",id=\"00000000-0000-0000-0000-000000000000\"} 400\n"
        ));
        assert!(out.contains("# TYPE huddle_peer_bitrate_in_bps gauge\n"));
        assert!(out.contains("huddle_peer_rtt_ms{room=\"ABCD-EFGH-JKLM\",peer=\"alice\",id=\"00000000-0000-0000-0000-000000000000\"} 42\n"));
        assert!(out.contains("huddle_peer_ingress_loss_fraction{room=\"ABCD-EFGH-JKLM\",peer=\"alice\",id=\"00000000-0000-0000-0000-000000000000\"} 0.01\n"));
        assert!(out.contains("huddle_room_participants{room=\"ABCD-EFGH-JKLM\"} 1\n"));
    }

    #[test]
    fn unknown_values_are_omitted_not_zeroed() {
        let out = render(&sample_report(None, None));
        assert!(!out.contains("huddle_peer_rtt_ms{"));
        assert!(!out.contains("huddle_peer_ingress_loss_fraction{"));
        // Header still declares the family; samples appear once measured.
        assert!(out.contains("# TYPE huddle_peer_rtt_ms gauge\n"));
    }

    #[test]
    fn renders_simulcast_layer_preferences() {
        let out = render(&sample_report(None, None));
        assert!(out.contains("# TYPE huddle_peer_simulcast_layer gauge\n"));
        assert!(out.contains(
            "huddle_peer_simulcast_layer{room=\"ABCD-EFGH-JKLM\",peer=\"alice\",id=\"00000000-0000-0000-0000-000000000000\",origin=\"bob\"} 0\n"
        ));

        // The high layer renders as 1, and a peer with no camera
        // subscriptions emits no samples at all (headers only).
        let mut report = sample_report(None, None);
        report.rooms[0].peers[0].simulcast_layers[0].layer = VideoLayer::High;
        let out = render(&report);
        assert!(out.contains("origin=\"bob\"} 1\n"));
        report.rooms[0].peers[0].simulcast_layers.clear();
        let out = render(&report);
        assert!(!out.contains("huddle_peer_simulcast_layer{"));
    }

    #[test]
    fn label_values_are_escaped() {
        let mut report = sample_report(None, None);
        report.rooms[0].peers[0].name = "a\"b\\c\nd".into();
        let out = render(&report);
        assert!(out.contains("peer=\"a\\\"b\\\\c\\nd\""));
    }
}
