//! The phone hub: an authenticated WebSocket API for phones on `127.0.0.1`, over the
//! sessions it finds, with paired devices, pairing secrets, the tailnet owner and an audit log.
#![forbid(unsafe_code)]
mod owner;
mod server;
mod sessions;
mod store;
mod tail;
pub use owner::{owner_login, tailnet_url};
pub use server::Hub;
pub use store::{Device, HubConfig, Store};
