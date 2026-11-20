//! The phone hub: an authenticated WebSocket API for phones on `127.0.0.1`, over the
//! sessions it finds, with paired devices, pairing secrets, the tailnet owner and an audit log.
#![forbid(unsafe_code)]
mod activity;
pub mod apns;
mod git;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the relay link (plan Task 6) is the first caller outside the tests"
    )
)]
mod keys;
mod launch;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the pairing flow (plan Task 6) is the first caller outside the tests"
    )
)]
mod link;
mod owner;
mod push;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the relay link (plan Task 6) is the first caller outside the tests"
    )
)]
mod secure;
mod server;
mod sessions;
mod store;
mod tail;
mod watch;
pub use owner::{owner_login, published_url};
pub use server::{Hub, Publisher};
pub use store::{ActivityTokens, DEFAULT_PORT, DEFAULT_RELAY, Device, HubConfig, Store};
pub use watch::{Lines, NOT_RUNNING, plain, watch};
