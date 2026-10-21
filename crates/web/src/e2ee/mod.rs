//! The client side of the E2EE key exchange (issue #27, ADR 010).
//!
//! [`safety`] and the framing half of [`keying`] are pure Rust so the
//! derivations are unit-testable natively; [`identity`] and [`session`]
//! are wasm-only — they wrap WebCrypto and IndexedDB, which only exist
//! in the browser. The media transform worker that consumes these keys
//! is #28's scope.

pub mod keying;
pub mod safety;

#[cfg(target_arch = "wasm32")]
pub mod identity;

#[cfg(target_arch = "wasm32")]
pub mod session;
