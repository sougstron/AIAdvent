# ask

Minimal Rust CLI that sends a question to an LLM and prints the answer.

Default backend: **Yolo-Auto** (`https://yolo-auto.com/v1`, model `qwen3.8-27b`),
the same OpenAI-compatible provider configured in pi (`~/.pi/agent/models.json`).

## Build

```sh
cargo build --release
```

## Launch from terminal

```sh
# question as arguments
./target/release/ask "What is the capital of France?"

# or via cargo
cargo run --release -- "What is the capital of France?"

# or pipe via stdin
echo "Explain Rust ownership in one sentence" | ./target/release/ask
```

## API key

Resolution order:

1. `$YOLO_API_KEY` environment variable
2. `apiKey` of the `Yolo-Auto` provider in `~/.pi/agent/models.json` (pi's config)

Optional overrides: `$YOLO_BASE_URL`, `$YOLO_MODEL`.
