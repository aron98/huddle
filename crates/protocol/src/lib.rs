//! The huddle wire protocol.
//!
//! Every message that crosses the signaling WebSocket or the REST API is
//! defined here. This crate is compiled into both the server and the WASM
//! client, so interface drift is a compile error on one side or the other
//! (see `docs/ARCHITECTURE.md`).
