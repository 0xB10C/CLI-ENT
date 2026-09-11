//! Integration tests against a real regtest `bitcoind`, asserting via RPC
//! (PLAN.md §15). `#[ignore]` by default; run in CI with the `download` feature.
//! Implemented in milestone 11.

#[allow(dead_code)]
mod common;
