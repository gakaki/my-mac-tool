# Project UI Rules

- Use `nsh.dev` Native Shell pure Rust layout only.
- Do not add or use TSX, JSX, React, or JavaScript UI entry points.
- Keep the user interface in `toolchain-installer/src/lib.rs` using
  `nsh-core`, `nsh-base`, and the macOS Native Shell platform crate.
- Preserve the existing shadcn-style Native Shell visual language:
  light cards, restrained borders, muted labels, and primary dark buttons.
- Keep a single user-facing GUI. The `toolchain-installer` binary is an
  internal helper only and must not expose another application UI.
- Do not reintroduce BetterDisplay, GPUI, egui, Electron, or a persistent
  background UI framework.
- Port the former `uuremote监视重启` behavior into the Native Shell Rust UI;
  do not keep a second Swift/AppKit GUI or embed the old application.
- Process monitoring must use exact executable matching, missing-process
  confirmations, launch grace, restart cooldown, SIGTERM, then SIGKILL.
- Auto-kill rules must persist, support search and multi-select, expose
  create/edit/delete/save actions, and support dragging a process into the
  rule list.
- Startup cleanup must only remove matching user/global LaunchAgents and
  LaunchDaemons; never remove unrelated login items or system components.
- Every tool installer must perform a filesystem-only preflight before any
  network request. One-click installs must preflight components in parallel,
  mark installed tools as skipped, and only download missing components.
- Individual tool tabs must expose the same fast check-and-install path.
- Run `cargo test --bins --lib` or the Native Shell equivalent before
  committing.
