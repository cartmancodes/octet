//! Fleet workflows: a YAML-defined sequence of vendor sessions (research,
//! review, implement, test) that Octet drives under approval gates.
#![forbid(unsafe_code)]

pub mod definition;
pub mod record;
pub mod state;
pub mod validate;
