//! The per-platform WebView capability matrix, as data (ADR 014).
//!
//! This is the recorded result of the issue #18 spike: Tauri renders
//! through the system WebView, and a video-conference shell lives or dies
//! by that engine's `RTCPeerConnection` / `getUserMedia` /
//! `getDisplayMedia` support. The matrix below is *researched
//! documentation*, not a runtime probe — see the ADR for sources and the
//! README for the manual verification checklist these findings still
//! require.

/// The desktop platforms the shell can be built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// WebKitGTK (`webkit2gtk-4.1`) via wry.
    Linux,
    /// WKWebView via wry.
    Macos,
    /// Microsoft Edge WebView2 via wry.
    Windows,
}

/// How complete a feature is in a platform's system WebView.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Works in current engine versions (still subject to the manual
    /// verification checklist).
    Yes,
    /// Implemented but with known gaps or fragility; degrade gracefully.
    Partial,
    /// Not available; the shell degrades gracefully.
    No,
}

impl Support {
    /// Whether a feature can be relied on at all.
    pub fn usable(&self) -> bool {
        matches!(self, Support::Yes | Support::Partial)
    }
}

/// One platform's media/WebRTC support, with the caveat that matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformCapability {
    /// Camera + microphone via `getUserMedia`.
    pub camera_mic: Support,
    /// Screen share via `getDisplayMedia`.
    pub screen_share: Support,
    /// `RTCPeerConnection` — the make-or-break feature for calls.
    pub webrtc: Support,
    /// The one-line story for the README/ADR table.
    pub note: &'static str,
}

/// The researched matrix (ADR 014 has the sources; see module docs).
///
/// Summary of findings:
/// - **Linux (WebKitGTK)**: capture APIs exist (PipeWire/xdg-portal, with
///   engine settings the embedder must enable), but `RTCPeerConnection`
///   does not ship in *stable* WebKitGTK tarballs — upstream plans
///   LibWebRTC enabled-by-default for the 2.56 cycle (~March 2027). Calls
///   in the shell on stock Linux are therefore not expected to work yet.
/// - **macOS (WKWebView)**: camera/mic work (permission delegate +
///   Info.plist usage strings required), `RTCPeerConnection` works, but
///   `getDisplayMedia` is broken in wry-based apps on Sonoma+ (the
///   permission prompt is suppressed → `NotAllowedError`).
/// - **Windows (WebView2)**: the strong platform — camera/mic and
///   `RTCPeerConnection` work; screen share uses the OS-level picker and
///   the host can intercept it via `ScreenCaptureStarting`.
pub fn platform_capability(platform: Platform) -> PlatformCapability {
    match platform {
        Platform::Linux => PlatformCapability {
            camera_mic: Support::Partial,
            screen_share: Support::Partial,
            webrtc: Support::No,
            note: "capture via PipeWire works once enabled, but stable WebKitGTK \
                   ships no RTCPeerConnection until the LibWebRTC default lands \
                   (2.56 cycle, ~March 2027)",
        },
        Platform::Macos => PlatformCapability {
            camera_mic: Support::Yes,
            screen_share: Support::No,
            webrtc: Support::Yes,
            note: "camera/mic + calls work (needs the permission delegate and \
                   Info.plist usage strings); getDisplayMedia is a known \
                   WKWebView/wry gap",
        },
        Platform::Windows => PlatformCapability {
            camera_mic: Support::Yes,
            screen_share: Support::Yes,
            webrtc: Support::Yes,
            note: "WebView2 (Chromium): calls, camera and the OS screen picker \
                   all work; best-supported platform",
        },
    }
}

/// Whether two-party calls are expected to work on a platform — the issue
/// #18 acceptance spike, answered from research. Note this is a *state of
/// the world* function, not a guarantee: the manual checklist in the
/// desktop README must still pass on real hardware.
pub fn calls_expected(platform: Platform) -> bool {
    platform_capability(platform).webrtc.usable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_is_the_strong_platform() {
        let caps = platform_capability(Platform::Windows);
        assert_eq!(caps.camera_mic, Support::Yes);
        assert_eq!(caps.screen_share, Support::Yes);
        assert_eq!(caps.webrtc, Support::Yes);
        assert!(calls_expected(Platform::Windows));
    }

    #[test]
    fn macos_supports_calls_but_not_screen_share() {
        let caps = platform_capability(Platform::Macos);
        assert_eq!(caps.camera_mic, Support::Yes);
        assert_eq!(caps.screen_share, Support::No);
        assert!(calls_expected(Platform::Macos));
    }

    #[test]
    fn linux_is_the_weak_spot_calls_not_expected_yet() {
        let caps = platform_capability(Platform::Linux);
        assert_eq!(caps.webrtc, Support::No);
        assert!(!calls_expected(Platform::Linux));
        assert_eq!(caps.camera_mic, Support::Partial);
        assert!(caps.note.contains("LibWebRTC"));
    }

    #[test]
    fn every_platform_has_a_note_for_the_readme_table() {
        for platform in [Platform::Linux, Platform::Macos, Platform::Windows] {
            let caps = platform_capability(platform);
            assert!(caps.note.len() > 20, "note missing for {platform:?}");
        }
    }

    #[test]
    fn usable_matches_yes_and_partial() {
        assert!(Support::Yes.usable());
        assert!(Support::Partial.usable());
        assert!(!Support::No.usable());
    }
}
