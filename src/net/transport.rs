//! The transport abstraction (PLAN.md §5).
//!
//! `Transport` is an enum over the v1 and v2 transports with raw-byte access for
//! misbehaviour. `Wire` carries the bytes and parsed message for the ring buffer.
//! Implemented in milestone 1 (v1) and milestone 3 (v2).
