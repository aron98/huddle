//! The client side of the E2EE key exchange (issue #27, ADR 010) and
//! its consumers: the media transform worker and encrypted chat
//! (issue #28, ADR 011).
//!
//! [`safety`], [`sframe`] and the framing halves of [`keying`] and
//! [`chat`] are pure Rust so the derivations are unit-testable
//! natively; [`identity`], [`session`] and [`transform`] are wasm-only —
//! they wrap WebCrypto, IndexedDB and the RTCTransform APIs, which
//! only exist in the browser.

pub mod chat;
pub mod keying;
pub mod safety;
pub mod sframe;

#[cfg(target_arch = "wasm32")]
pub mod identity;

#[cfg(target_arch = "wasm32")]
pub mod session;

#[cfg(target_arch = "wasm32")]
pub mod transform;
