# ask

A ChatGPT-style chat client for the terminal — a Rust/ratatui TUI in the
spirit of the opencode/pi/omp consoles, talking to an OpenAI-compatible
endpoint. Default backend: **Yolo-Auto** (`https://yolo-auto.com/v1`, model
`qwen3.8-27b`).

## Build

```sh
cargo build --release
```

## Quick start

```sh
./target/release/ask                              # open the chat TUI
./target/release/ask "what is the capital of France?"   # one-shot question
./target/release/ask --sessions                   # list saved chat sessions
./target/release/ask --verify-stop --budget-tokens 60    # prove the stop condition works
```

Piping a question on stdin still works, same as a one-shot call:

```sh
echo "Explain Rust ownership in one sentence" | ./target/release/ask
```

## The chat TUI

```
Enter          send a message, or run a /command
Shift+Enter    insert a newline (the input box grows up to 4 lines as it wraps)
←→ ↑↓          move the cursor through the input text
Tab            toggle focus between the input and the settings panel
↑↓ / j k       (settings/sessions panel) move the selection
←→ / h l       (settings panel) change the selected setting
d / Delete     (sessions panel) delete the selected chat, with y/n confirmation
Ctrl-N         start a new chat session
PgUp / PgDn    scroll the conversation
↑ / ↓          with the cursor on the input's first/last line: scroll one line
mouse wheel    scroll the conversation
Home / End     jump to the top / bottom of the conversation
Esc            stop a running generation; otherwise close a panel or quit from the input
Ctrl-Q         quit, even while a generation is running
```

Pressing Esc while a reply is streaming stops it immediately: whatever text
already arrived stays in the transcript (and the saved session) marked
`[stopped by Esc]`; if nothing had arrived yet, the question is dropped
entirely. The same works for any spinner-backed call (`/verify`,
`/personas`, JSON-mode replies, `/json edit`).

Scrolling up pauses the "follow the tail" behavior while a reply streams in;
scrolling back to the bottom (or sending a new message) re-pins it.

Telling the terminal apart for Shift+Enter requires the kitty keyboard
protocol; the app requests it on startup. On terminals that support it
(kitty, alacritty, ghostty, wezterm, foot, contemporary tmux with
extended-keys, …) Shift+Enter inserts a newline; elsewhere it degrades to a
plain Enter.

### Slash commands

```
/new                              start a new chat session
/sessions                         list and switch between saved sessions
/effort [none|low|medium|high]    get or set reasoning effort
/json on|off                      toggle structured JSON output
/json fields a,b,c                set a flat schema with these string fields
/json schema <json>                set a full JSON Schema
/json edit <instruction>          ask the model to rewrite the schema
/json show                        print the active schema
/temp [off|0.0-2.0]               get or set sampling temperature (2.0 is the provider's max)
/stop add <seq>                   add a stop sequence (max 4)
/stop clear                       clear stop sequences
/verify [prompt]                  prove the stop condition changes the output
/personas <question>              ask physicist/philosopher/mathematician, one call each, in sequence
/personas a,b,c: <question>       same, with your own cast instead of the default three
/settings                         open the settings panel (Tab does the same)
/quit                             exit
```

While a reply is in flight, the transcript shows an inline animated indicator
(`/ - \ -`, cycled on a background thread) in place of the eventual answer,
instead of a full-screen "working" overlay that used to blank out the rest of
the conversation.

## Sessions

Every chat is a session: an ordered list of messages plus the settings that
were active. `/new` starts one; `/sessions` lists every session saved so far
(most recent first) and lets you switch. Sessions are written to
`~/.ask/sessions/<id>.json` after every turn, so old chats survive a restart.
Override the location with `$ASK_SESSIONS_DIR` (used by the test suite).

Press `d` (or Delete) on a highlighted session in the `/sessions` list to
delete it — a confirmation prompt (`y`/`n`) guards against accidental loss.
Deleting the chat currently open starts a fresh one in its place, so the
next autosave doesn't recreate the file you just removed.

## Reasoning effort

`/effort none|low|medium|high` maps directly onto the provider's
`reasoning_effort` field. `none` additionally sends
`chat_template_kwargs.enable_thinking: false` — verified live against this
provider to be the only combination that reliably zeroes out
`reasoning_tokens` (see `api::tests::effort_none_disables_thinking_switches`).

## JSON mode

`/settings` toggles structured output on or off; `/json` edits *what* gets
filled in:

- `/json fields title,game,publisher,summary` builds a flat, all-string
  schema — the common case: ask a question, get the answer back as one
  `key: value` line per field, not a raw JSON blob.
- `/json schema <json schema>` sets an arbitrary JSON Schema (validated
  before it's accepted).
- `/json edit <instruction>` asks the model itself to rewrite the current
  schema in natural language (e.g. `/json edit add a "price" field`) — the
  reply is parsed and schema-validated before being applied, so a bad
  response is rejected rather than silently corrupting the session.

Under the hood this is `response_format: json_schema, strict: true`, so the
shape is enforced by the provider, not just requested. On the CLI and in the
TUI, the reply is flattened into `Key: value` lines for display; the same
text is what's stored as the assistant's turn, so multi-turn JSON
conversations stay consistent between what's shown and what's replayed.

## Length cap (`max_chars`)

A hard character limit on the visible answer, set in `/settings`. It's
enforced twice:

1. **Soft** — a system-prompt note asking the model to wrap up within the
   budget, so it doesn't get cut off mid-sentence more than necessary.
2. **Hard** — the reply is truncated client-side to exactly `max_chars`
   characters (`api::enforce_max_chars`), regardless of what the model
   actually produced. This is what makes the cap a guarantee rather than a
   suggestion — see `api::tests::max_chars_truncates_deterministically`.

## Stop condition — design and proof

The task this answers: make the model stop reasoning/generating at a chosen
point, and *prove* it actually happens rather than trusting a flag.

Two independent, provider-level levers, both configurable from `/settings`:

- **`budget_tokens`** → `max_tokens` on the request. Counts reasoning *and*
  visible tokens together, so it directly caps "how much the model is
  allowed to think and write" before it's cut off mid-generation
  (`finish_reason=length`).
- **stop sequences** (`/stop add <seq>`) → the provider's `stop` field. The
  instant the model emits one of these literal strings, generation halts
  (`finish_reason=stop`) — useful when you want a structural cutoff (e.g.
  after the first paragraph, on a blank-line separator) rather than a raw
  token count.

Both were confirmed live against `yolo-auto.com` before being wired in:
with `reasoning_effort=none` (so budget/stop apply to the visible answer
instead of being eaten by hidden reasoning), a small `max_tokens` truncates
exactly at that count, and a stop sequence like `"\n\n"` cuts a multi-item
list down to the first item.

**`/verify [prompt]`** is the built-in proof: it sends the *same* prompt
twice — once with the stop condition stripped, once with your current
settings — and reports `finish_reason`, token/char counts and both texts
side by side. It only reports "CONFIRMED" when the "on" run shows the
mechanical signature of whichever lever is configured (budget:
`finish_reason=length` with `completion_tokens` at the budget; stop:
`finish_reason=stop` with a shorter answer than the unconstrained run) —
not just "the text looks different", since two unconstrained calls to a
non-deterministic model differ anyway. If neither lever is set, `/verify`
refuses to run rather than print a misleading result. Same check is
available non-interactively:

```sh
ask --verify-stop --budget-tokens 60
ask --verify-stop --stop $'\n\n' "List 6 things, one per paragraph."
```

## Personas — sequential, independent multi-perspective replies

`/personas <question>` runs the same question past several personas — by
default a physicist, a philosopher and a mathematician — and shows each
answer as it comes in. Each persona only ever sees the original question
(never the other personas' replies), so these are genuinely independent
takes, not one model role-playing through several voices in a single
response.

Calls are made **one at a time, never in parallel**: the provider handles
concurrent requests from one client poorly, so `/personas` waits for each
persona to finish before starting the next, showing which persona is
currently running (`thinking as philosopher (2/3)…`) in the inline spinner.

Custom cast: `/personas physicist,poet,lawyer: is it ethical to lie to
protect someone's feelings?`

## Temperature

Sampling temperature: `/temp 1.2` (`/temp off` to clear), the `/settings`
panel, or `--temperature F` for one-shot calls. Cycled in the TUI over
`off, 0.0, 0.3, 0.7, 1.0, 1.2, 1.5, 2.0`, and shown in the header strip as
`temp=…` so it is visible that it is actually set.

**Ceiling: `2.0`.** `yolo-auto.com` validates the field itself and rejects
anything outside `[0.0, 2.0]` with `HTTP 400 — temperature: Validation
error: range [min 0.0, max 2.0]` (`2.01` is already refused). The client
checks the same range up front, so a bad value fails instantly with a plain
message instead of a wasted round trip.

The parameter is honored: at `0.0` the same prompt returns the same answer
run after run; at `1.2` it varies. But **on its own, temperature looks much
weaker than it is**, because the provider applies its own truncation
defaults (Qwen-style `top_k`/`top_p`) that cut the distribution down before
temperature ever widens it. Measured with "name one random animal", eight
runs each:

| settings | result |
| --- | --- |
| `temp=0.0` | same word 8/8 |
| `temp=1.2` | a handful of common animals, `Ostrich` still dominant |
| `temp=2.0` | slightly more spread, still all plausible animals |
| `temp=2.0 top_k=-1` | falls apart into other languages and junk tokens |

So the two truncation knobs are exposed as well — `top_p` and `top_k` rows in
`/settings`, `--top-p F` / `--top-k N` on the command line, both unset by
default so the provider's own defaults stay in place. `top_k=-1` (shown as
`full`) removes the cutoff entirely; that is the setting that makes a high
temperature behave the way people expect it to — including badly, at `2.0`.

## API key

Resolution order:

1. `$YOLO_API_KEY`
2. `apiKey` of the `Yolo-Auto` provider in `~/.pi/agent/models.json`

Optional overrides: `$YOLO_BASE_URL`, `$YOLO_MODEL`.

## Other one-shot flags

```
--effort none|low|medium|high
--json / --json-fields a,b,c / --json-schema-file PATH
--max-chars N
--budget-tokens N
--stop SEQ            (repeatable, max 4; \n and \t escapes work)
--temperature F        0.0-2.0 (the provider's own range)
--top-p F              nucleus cutoff; unset = provider default
--top-k N              top-k cutoff, -1 = off; unset = provider default
--raw                  print the full API response
--quiet                suppress the usage/finish_reason line
--verify-stop          run the stop-condition proof and exit
--sessions             list saved sessions and exit
```

Every flag has a matching `ASK_*` env var (`ASK_EFFORT`, `ASK_MAX_CHARS`,
`ASK_BUDGET_TOKENS`, `ASK_STOP`).

## Layout

```
src/
  main.rs      dispatcher
  cli.rs       clap + one-shot / --verify-stop / --sessions flow
  config.rs    Settings, Effort, JsonMode
  api.rs       request body, multi-turn chat(), effort->reasoning_effort, max_chars enforcement
  session.rs   session persistence (~/.ask/sessions/*.json)
  render.rs    JSON-mode flattening ("Key: value" lines) shared by CLI and TUI
  tui.rs       the chat TUI: transcript, input, slash commands, settings/sessions panels
  verify.rs    the stop-condition self-test
```

`Outcome.content` is `Option<String>` on purpose: when `max_tokens` (or a
stop sequence) fires before any visible token is produced, the provider
returns `content: null` — the model spent the whole budget in
`reasoning_content`.
