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

Since task-6, personal provider keys (glm / deepseek / openrouter) are never
baked into the app: `ask --login <provider>` live-checks a key against the
provider and stores it in `~/.ask6/auth.json` (0600). `ask --keys` shows
every provider's source, `ask --verify-login` rechecks them live. Resolution
order: env var → `~/.ask6/auth.json`. Details: `tree/task-7/README.md`,
раздел «Логин».

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

**The active snapshot is `tree/task-11/`.** It is a working agent: a ratatui
chat TUI over z.ai's plain OpenAI-compatible API, built around a first-class
`Agent` entity (settings, history, AGENTS.md in the system message) rather
than a bare HTTP call. Default and only live model is `glm-5.3-flash`; the
rest of the catalog is selectable but refused at send time because those
ids cost money. Tasks 9–10 built **five context strategies behind one switch**
(`off` / `summary` / `window` / `facts` / `branch`; `/strategy`, the
`strategy` row in settings, `--strategy`, with `/compress` and `--compress`
as aliases), proved by `ask --verify-compress` and `ask --verify-context
window|facts|branch|all`. Task 11 added a sixth value, `memory`, and with it
an **explicit three-layer memory model** (`memory.rs`): short (current
dialogue), working (current task) and long (profile, decisions, knowledge).
The layers are physically separate — one folder and one file each under
`tree/task-11/memory/{short,working,long}/` — and every record carries who
wrote it and *why it landed in that layer*. Nothing is saved "to memory": the
layer is chosen explicitly, either by hand (`/mem <layer> set k v`) or by
`memory::route`, where a key prefix (`профиль.` / `задача.` / `тема.`) beats
the extractor's own suggestion and the disagreement is surfaced, not
swallowed. On the wire the layers are three separate `system` blocks plus the
last N messages, which is what makes one-layer-at-a-time attribution
possible. The proof is `ask --verify-memory routing|influence|isolation|all`
(`--offline` runs the half that needs no network) and it is causal, never
"the texts differ": `routing` reads the layer files back from disk, `influence`
is Confirmed only when removing **one** block (long) kills the profile answer
and leaves the task answer, and `isolation` only when switching tasks forgets
the working code while the long-term one survives and the old task's file
still holds it. All three came back Confirmed live on `glm-5.3-flash`. Its
`README.md` covers the Agent shape, key resolution, runtime settings, all six
strategies plus the memory model with their corner cases and real verify
output, and the live lever self-test (temperature confirmed; top_p flat;
top_k unsupported). It started as a verbatim copy of `tree/task-10/`, which
came from `tree/task-9/` and, before that, `tree/task-8/` / `tree/task-7/`
(task 7 already met the requirements, so it stayed frozen); `tree/task-5/`
before that was the *Model Ladder* state. Everything below in this file describes the older TUI that still
lives in the repo root `src/`.

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
