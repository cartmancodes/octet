//! Release tests for `octet fleet`, numbered as the spec's "Release tests":
//! `octet fleet` runs as a child process against a real git work tree, with
//! the scripted fake vendors (`protocol-child`) in every step: the Claude
//! dialect researching, planning and reviewing, the Codex dialect coding and
//! testing.
//!
//! Each test has its own directory: the work tree `work/`, the data directory
//! `data/`, the fleet script and its turn log. The script is named to the
//! `octet` process (`OCTET_FLEET_SCRIPT`), and every vendor inherits it
//! through the gate. What a test asserts it reads from the run's log, its
//! artifacts and the fake's turn log, never from timing.
//!
//! Every wait is bounded and fails the test with what was seen. Every `octet`
//! a test starts leads its own process group, killed if it is still running
//! when the test lets go of it; when the test ends, however it ends, every
//! process still carrying one of its run markers is killed, and so is every
//! survivor its fakes started.
#![expect(
    clippy::unwrap_used,
    reason = "test code: an unwrap that fails is the test failing"
)]

mod common;
mod crashes;
mod interrupts;
mod ownership;
mod pauses;
mod run_and_evidence;
