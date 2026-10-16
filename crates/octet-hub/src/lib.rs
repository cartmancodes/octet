//! The phone hub: an authenticated WebSocket API for phones on `127.0.0.1`, over the
//! sessions it finds, with paired devices, pairing secrets, the tailnet owner and an audit log.
#![forbid(unsafe_code)]
mod activity;
pub mod apns;
mod git;
mod launch;
mod owner;
mod push;
mod server;
mod sessions;
mod store;
mod tail;
mod watch;
pub use owner::{owner_login, published_url};
pub use server::{Hub, Publisher};
pub use store::{ActivityTokens, Device, HubConfig, Store};
pub use watch::{Lines, NOT_RUNNING, plain, watch};
