//! The huddle media plane (SFU).
//!
//! Sans-io by design: the entire crate is built on `str0m` and is
//! unit-testable without sockets — two `Rtc` instances can be wired together
//! in-process (see `docs/adr/001-sfu-on-str0m.md`).
