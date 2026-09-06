# CLAUDE.md

See `AGENTS.md` for build/test/lint commands, live-verification steps, TUI
smoke-testing notes, and module layout — it applies here unchanged.

Project-specific notes for Claude Code:

- This is the `ask` chat TUI (Rust/ratatui), not the old gamedev/news/comparison
  tool — that theme was dropped entirely; don't resurrect `news.rs`/`compare.rs`/
  `schemas/gamedev.json` patterns from old history.
- Always run `cargo test` and `cargo clippy --all-targets` before considering a
  change done; both must be clean.
- Changes to `src/tui.rs` (scroll, layout, panels) need a live tmux smoke test,
  not just a compile check — see AGENTS.md's "real bug twice" note on scroll
  clamping. The same rule applies to `tree/task-5/ui/`: check it in a real
  window (the launch incantation is in that folder's README), because
  WebKitGTK here renders a blank page unless the DMABUF/compositing env vars
  are set, and silently ignores `<select>` colors.
- There is a single current global task: the active snapshot folder lives
  under `tree/` and is named in `docs/CurrentTask.md`. Work only in that
  folder; don't touch the rest of the code (root `src/`, other snapshots).
- The stop-condition proof (`verify.rs`, `/verify`, `--verify-stop`) is the
  feature the user cares most about being *actually true*, not just plausible.
  If you touch it, re-run it live and confirm the causal signature
  (`finish_reason` + token/char counts), not just that output text changed.
  `tree/task-5/src/verify.rs` is the same idea applied to the model ladder:
  it only reports "confirmed" when each platform echoes the model that was
  actually asked for *and* the top rung outscores the bottom one on questions
  with a machine-checkable numeric answer. Don't weaken it to "the texts
  differ" — the earlier form of this check is what caught Z.AI dropping
  `temperature`, and the current form honestly reports `Flat` rather than
  claiming an ordering it did not measure.
