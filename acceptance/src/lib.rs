//! Acceptance against real Telegram on a Test account (ADR-0009).
//!
//! Scenarios run in a [`world::World`]: the Fake world (the real server on a
//! fake Telegram, in-process, in `cargo test`) or the Real world (the Test
//! account, its Driver and the Peer, through `cargo accept`).

pub mod blocked;
pub mod config;
pub mod fake;
pub mod fixtures;
pub mod forms;
pub mod lease;
pub mod login;
pub mod real;
pub mod run;
pub mod scenarios;
pub mod telegram;
pub mod world;
