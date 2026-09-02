# ask

Rust CLI that talks to an OpenAI-compatible endpoint with **three independent
response controls**: output format, length, and stop sequences.

With no question it is a **game-development news digest**: it fetches real news
from Steam (and optionally Hacker News) and asks the model to summarise them.
The interesting part is not the news — it is that the same material can be
forced into a stable JSON shape, cut short, or stopped on a delimiter.

Default backend: **Yolo-Auto** (`https://yolo-auto.com/v1`, model `qwen3.8-27b`).

## Build

```sh
cargo build --release
```

## Quick start

```sh
# original behaviour: unconstrained pass-through question
./target/release/ask "What is the capital of France?"

# news digest as strict JSON (default when no question is given)
./target/release/ask --preset strict

# the same digest, but cap the list at 3 entries via the schema
./target/release/ask --preset capped-items

# compare every preset and print a stability report
./target/release/ask --compare

# interactive switches
./target/release/ask --tui
```

Pipe a question on stdin the same way as before:

```sh
echo "Explain Rust ownership in one sentence" | ./target/release/ask
```

## Response controls

| Knob | Flag | What it does |
|---|---|---|
| Format | `--format text\|json\|schema` | `text` — free prose. `json` — `response_format: json_object` (valid JSON, keys are the model's). `schema` — `json_schema` + `strict: true` (the shape is ours). |
| Length (tokens) | `--max-tokens N` | Hard generation budget. **Also counts reasoning tokens.** Truncation yields `finish_reason=length` and often invalid JSON. |
| Length (semantic) | `--max-items N` | Writes `maxItems` into the topic schema. Cuts whole entries instead of mid-word. |
| Stop | `--stop SEQ` (repeatable, max 4) | Provider stop sequences. `\n` and `\t` escapes work. |
| Thinking | `--think on\|off` | The default model is a reasoning model. Thinking **must be off** for length/stop to apply to the visible answer — otherwise they fire inside `reasoning_content` and `content` comes back `null`. |

A named `--preset` sets a combination; any flag after it overrides.

```
ask --list-presets
```

| Preset | format | max_tokens | max_items | stop | think |
|---|---|---|---|---|---|
| `baseline` | text | — | — | — | on |
| `json-soft` | json_object | — | — | — | on |
| `strict` | json_schema | — | — | — | off |
| `capped` | json_schema | 300 | — | — | off |
| `capped-items` | json_schema | — | 3 | — | off |
| `stopped` | text | 600 | — | `\n---\n` | off |
| `strict-all` | json_schema | 1600 | 5 | `\n\n\n` | off |

## News service

No question ⇒ topic `gamedev`. Real material is fetched up front (once per
process, so `--runs` / `--compare` stay fair) from:

- **Steam** `ISteamNews` — no API key, unix timestamps, curated appids
  (CS2, Dota 2, Cyberpunk 2077, Elden Ring, Valheim, …)
- **Hacker News** via Algolia — `query=game development`

```sh
ask --source steam --since 7d --limit 8          # default-ish
ask --source both --since 48h
ask --source none                                # model invents the items
```

`--since` accepts `48h`, `7d`, `2w`, or a bare number of hours.

The schema lives in `schemas/gamedev.json` and is sent to the provider as
`response_format.json_schema` with `strict: true`. Shape:

```
{ period, generated_at, headline,
  items[{ title, game, studio, platforms[], category, date, summary, hype_score }],
  top_pick }
```

`platforms` is an enum, `hype_score` is `1..10`. `--max-items N` patches
`items.maxItems` before the request goes out.

A free-form question with no `--topic` still behaves like the original CLI
(plain text, thinking on, no news fetch).

## Comparison

`--compare` runs every preset (or `--presets a,b`) `--runs` times (default **5**)
against the same fetched material and prints a Markdown table:

- JSON validity
- schema match
- number of distinct key-shapes (1 = stable)
- item counts
- completion tokens (min/med/max)
- `finish_reason` distribution

Raw responses land in `runs/<timestamp>/` together with `report.md`.
`--out-dir` / `--no-save` override that.

### Live run (qwen3.8-27b @ yolo-auto.com, 3 runs, 8 Steam items / 7 days)

| preset | constraints | JSON ok | schema ok | shapes | items | completion tok (min/med/max) | finish_reason |
| --- | --- | --- | --- | --- | --- | --- | --- |
| baseline | text, think on | 0/3 | 0/3 | n/a | — | 1376/1395/1430 | stop×3 |
| json-soft | json_object, think on | 3/3 | 0/3 | **3 (drift)** | — | 1844/2424/2488 | stop×3 |
| strict | json_schema, think off | **3/3** | **3/3** | **1 (stable)** | 7 | 1194/1194/1269 | stop×3 |
| capped | schema + max_tokens=300 | 0/3 | 0/3 | n/a | — | 300 | **length×3** |
| capped-items | schema + maxItems=3 | **3/3** | **3/3** | **1 (stable)** | **3** | 583/655/664 | stop×3 |
| stopped | text + stop=`\n---\n` | 0/3 | 0/3 | n/a | — | **27/29/65** | stop×3 |
| strict-all | schema + max_tokens=1600 + maxItems=5 + stop | **3/3** | **3/3** | **1 (stable)** | **5** | 827/915/932 | stop×3 |

What that shows, in one paragraph:

- **Unconstrained text** is long and a different shape every time.
- **`json_object` is not enough.** All three runs parsed as JSON, but none
  matched the schema: run 1 wrapped everything in `digest`, runs 2–3 used
  `entries` instead of `items`, and nested keys still drifted (3 distinct
  shapes).
- **`json_schema` + `strict` + thinking off** produced the same key set
  (`period`, `generated_at`, `headline`, `items[]`, `top_pick`) on every run.
- **`max_tokens=300` always truncated mid-string** (`finish_reason=length`,
  `EOF while parsing`). Token limits are a blunt instrument on JSON.
- **`maxItems=3` shortened the answer semantically**: still valid, still the
  same shape, exactly three entries.
- **Stop sequence `\n---\n`** cut the text digest to ~30 tokens (the headline
  plus at most the first entry) versus ~1400 unconstrained.
- Stacking all three knobs works **only if the token budget is above the
  schema's natural size**. `max_tokens=700` with `maxItems=5` truncated like
  `capped`; `1600` completed with exactly 5 valid items.

The model is a reasoning model. `--think on` (the `baseline` / `json-soft`
presets) spends time in `reasoning_content`; `--think off` sends
`reasoning_effort=none` and `chat_template_kwargs.enable_thinking=false`,
which is what makes length and stop predictable.

## TUI

```sh
ask --tui
```

One screen, no logic of its own — it edits the same `RunConfig` and calls the
same `Engine` as the CLI.

```
↑↓ / j k     select a setting
←→ / h l     change it (presets, format, think, numbers, stop, source)
Enter        generate
c            compare every preset
f            refetch news
r            toggle raw API response
PgUp / PgDn  scroll the answer
q / Esc      quit
```

## Other flags

```
--topic gamedev|none     topic / schema (default: gamedev if no question)
--schema-file PATH       override the built-in schema
--runs N                 repeat the request (stability check)
--temperature F          sampling temperature
--raw                    print the full API response
--stats / --quiet        always / never print usage + finish_reason
--show-prompt            print the system/user messages and exit
--show-request           print the JSON body and exit
```

## API key

Resolution order:

1. `$YOLO_API_KEY`
2. `apiKey` of the `Yolo-Auto` provider in `~/.pi/agent/models.json`

Optional overrides: `$YOLO_BASE_URL`, `$YOLO_MODEL`.

Every CLI flag also has an `ASK_*` env var (`ASK_FORMAT`, `ASK_MAX_TOKENS`,
`ASK_STOP`, `ASK_THINK`, `ASK_TOPIC`, `ASK_SOURCE`, `ASK_SINCE`, …) so the
TUI and the CLI share one configuration.

## Layout

```
src/
  main.rs      dispatcher
  cli.rs       clap + non-interactive flow
  config.rs    RunConfig, Format, presets
  api.rs       request body, call, Option<String> content, finish_reason, usage
  engine.rs    fetch once, generate, judge (JSON / schema / key-signature)
  news.rs      Steam + Hacker News
  topics.rs    topic registry (schema + prompt)
  compare.rs   preset matrix, metrics, Markdown report
  tui.rs       ratatui settings screen
schemas/       JSON Schema files, embedded via include_str!
PLAN.md        design notes and the 10 topic candidates
```

`Message.content` is `Option<String>` on purpose: when `max_tokens` is eaten
by reasoning (or a stop sequence fires inside it) the provider returns
`content: null`. Treating it as `String` used to crash the original CLI.
