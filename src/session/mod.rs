//! The session task (PLAN.md §3, §6): owns the socket in a `select!` loop over
//! incoming bytes, commands, and timers; emits `Event`s and updates `SessionView`.

pub mod automations;
pub mod events;
pub mod handshake;
pub mod view;
