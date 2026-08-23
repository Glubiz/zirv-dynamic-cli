## Memory
- Key: dashboard-terminal-traps
- Written-by: setup-migration
- Written: 1787496000
- Verified: 1787496000
- Source: setup
- Tags: dashboard, terminal, gotcha, panic
- Paths: src/commands/ctx/dash/ui.rs, src/commands/ctx/dash/pane.rs, src/commands/ctx/term.rs

Ord::clamp panics when the area is 0, so use .max(1).min(x.max(1)) and guard renderers with Rect::is_empty -- the release profile is panic = abort, near a TUI. A full-screen child on the alternate screen has NO vt100 scrollback at any Parser::new setting. Never use crossterm::EnableMouseCapture (it forces ?1003 motion flood); write ?1000h?1002h?1006h raw via term::dash_mouse_on_bytes. Any overlay must render Clear first, or it silently eats every keystroke.
