//! The phone hub's private state: paired devices, pairing secrets, the
//! tailnet owner and an audit log of remote actions.
#![forbid(unsafe_code)]
mod owner;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the hub server, the next task, is the caller")
)]
mod sessions;
mod store;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the hub server, the next task, is the caller")
)]
mod tail;
pub use owner::{owner_login, tailnet_url};
pub use store::{Device, HubConfig, Store};
