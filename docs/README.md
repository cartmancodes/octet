# Octet documentation

## User guides

- [Using the terminal UI](tui.md): install, keys, commands, permission
  modes, switching models and providers, journals and limits.
- [Use Octet from your phone](remote-control.md): pair the phone app with
  `octet pair` in three steps, what the relay can and cannot see, revoking a
  phone, and the hub's security notes.
- [Running the relay](relay.md): hosting the relay that carries a phone's
  connection, for the owner or for self-hosting.
- [Persistent goals](rust/goals.md): how `/goal` keeps working across turns
  and how it decides it is done.
- [Fleet workflows](fleet/README.md): `octet fleet`, multi-step runs across
  providers (research, plan, implement, test, review) with approval gates,
  pauses and the guarantees a completed run carries.

## Reference

- [Features](rust/tui-features.md): what works today, feature by feature,
  and the preview's boundaries.
- [The protocol gate](rust/protocol-gate.md): checking a vendor CLI's wire
  protocol against the drivers with one live scenario (`make gate`).
- [Adding a provider](rust/adding-a-provider.md): how to support another
  vendor CLI: one file and one row in the provider table.
- [Parity matrix](rust/parity-matrix.md): the release requirements still
  open, measured against the earlier Python harness.
- [Mascot artwork](design/octet-agent-modes/): the pixel-art reference for
  Octet's nine poses.

## Development history

These are dated working notes kept for their reasoning. They describe the
code as it was when written, so the guides above win where they disagree.

- [Design specs](superpowers/specs/) and
  [implementation plans](superpowers/plans/), named by date.
- [Code reviews](reviews/), named by date.
- [Phone access evaluation](rust/remote-control-ssh.md), the analysis behind
  the tmux, SSH and mosh route (documentation only), and a
  [proposal for a browser-based remote client](rust/remote-control-plan.md)
  that has not been built.
- An earlier Python harness and web UI were removed on 2026-10-04. They
  remain in git history; the parity matrix names the last commit that has
  them.
