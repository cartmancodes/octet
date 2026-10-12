//! The phone hub's private state: paired devices, pairing secrets, the
//! tailnet owner and an audit log of remote actions.
#![forbid(unsafe_code)]
mod owner;
mod store;
pub use owner::{owner_login, tailnet_url};
pub use store::{Device, HubConfig, Store};
