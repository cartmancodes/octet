//! The phone hub: phones reach it through a relay, each over an end-to-end encrypted channel,
//! and it serves them the sessions it finds, with paired devices, pairing secrets and an audit
//! log. It listens on no network port.
#![forbid(unsafe_code)]
mod activity;
pub mod apns;
mod git;
mod keys;
mod launch;
mod link;
mod push;
mod relay;
mod secure;
mod server;
mod sessions;
mod store;
mod tail;
mod watch;
pub use server::Hub;
pub use store::{ActivityTokens, DEFAULT_RELAY, Device, HubConfig, Store};
pub use watch::{Lines, NOT_RUNNING, plain, watch};
