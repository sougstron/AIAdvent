# AGENTS.md

`ask` is a Rust/ratatui console chat TUI (ChatGPT-style) over an
OpenAI-compatible endpoint (default: Yolo-Auto). See `README.md` for the
full feature writeup (sessions, `/effort`, `/json`, length cap, stop
condition). This file is the quick-reference for working on the code.

## Build / test / lint

```sh
cargo build             # debug
cargo build --release   # release binary at target/release/ask
cargo test               # 21 unit tests, no network required
cargo clippy --all-targets   # must stay clean before committing
```

## Live verification

Network calls need an API key: `$YOLO_API_KEY`, or `apiKey` of the
`Yolo-Auto` provider in `~/.pi/agent/models.json` (see `api::resolve_api_key`).
With that in place:

```sh
./target/release/ask "one-shot question"
./target/release/ask --verify-stop --budget-tokens 60   # proves the stop condition works
./target/release/ask --verify-temp --temperature 2.0    # proves temperature changes the output
```

`/verify` (or `--verify-stop`) is the load-bearing check for the stop-condition
feature — it runs the same prompt with the lever on vs off and only reports
CONFIRMED when the causal signature (`finish_reason`, token/char counts)
actually shows the cutoff, not just "the text differs" (LLM sampling alone
can make two runs differ). Don't weaken that check to a text-diff heuristic.

## TUI smoke testing

Automated tests don't cover terminal rendering. When touching `src/tui.rs`,
smoke-test interactively (e.g. via `tmux new-session -d ... "./target/release/ask"`,
then `tmux send-keys` / `tmux capture-pane -p`). Anything that computes a
scroll offset must be clamped with `App::max_scroll` — `Paragraph::scroll`
clips rather than clamps, so an offset past the wrapped content renders a
blank pane (this has been the real bug twice: once on auto-scroll after a
reply, once on manual `PageDown`).

## Current global task

There is always exactly one active global task; app states are snapshotted
into `tree/task-*/` folders (source + release binary, each buildable on its
own). Which folder is active right now, and the rules for rotating to the
next state, are in `docs/CurrentTask.md` — work only in that folder and
don't touch the rest of the code.

## Layout

```
src/main.rs    dispatcher
src/cli.rs     clap args, one-shot / --verify-stop / --sessions flow
src/config.rs  Settings, Effort, JsonMode
src/api.rs     request body, multi-turn chat(), effort->reasoning_effort, max_chars enforcement
src/session.rs session persistence (~/.ask/sessions/*.json)
src/render.rs  JSON-mode flattening ("Key: value" lines), shared by CLI and TUI
src/tui.rs     the chat TUI: transcript, input, slash commands, settings/sessions panels
src/verify.rs  the stop-condition self-test
```

## Conventions

- `target/` is gitignored — do not commit build artifacts; `cargo build --release`
  is fast enough to run on demand.
- Prefer deterministic, client-side enforcement over prompt hints alone (see
  `api::enforce_max_chars`) — a limit the model merely gets asked to respect
  is not a guarantee.
- Keep JSON-reply flattening (`render.rs`) as the single shared implementation
  between the CLI one-shot path and the TUI; don't reintroduce a second copy.
