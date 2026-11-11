//! The Antigravity CLI (`agy`) as a provider.
mod guarded;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "called by the Antigravity protocol, which a later commit adds"
    )
)]
mod settings;
mod trust;
pub use trust::{
    WorkspaceConfig, antigravity_trust, antigravity_trusted, antigravity_workspace_config,
};
