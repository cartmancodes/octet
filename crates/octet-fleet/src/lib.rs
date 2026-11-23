//! Fleet workflows: a YAML-defined sequence of vendor sessions (research,
//! review, implement, test) that Octet drives under approval gates.
#![forbid(unsafe_code)]

pub mod contract;
pub mod definition;
pub mod evidence;
pub mod gate;
pub mod machine;
pub mod ownership;
pub mod record;
pub mod recover;
pub mod snapshot;
pub mod state;
pub mod store;
pub mod validate;

#[cfg(test)]
mod sim;
