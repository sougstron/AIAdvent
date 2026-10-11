# Home engine: qwen3:1.7b on the iGPU, nomic on CPU

Isolated Docker Compose project `ask-home-engine`, installed only into the new
`/opt/ask-home-engine` directory on `root@192.168.0.128`. No VM, CT, existing
Docker workload, host service or router configuration is changed.

## Current layout (TASK-120, 2026-10-11)

- `chat`: llama.cpp `llama-server` (Vulkan build, image pinned by digest
  `f05a83f1…`, b11552) runs `qwen3:1.7b` fully on the Radeon 680M
  (`-ngl 99`, 8192 context, flash attention, thinking off via the chat
  template). Only `/dev/dri/renderD128` is passed in (group `render`, 993);
  still unprivileged, read-only root, no capabilities. Weights + KV live in
  the 4 GiB BIOS VRAM carve-out (2.2 GB used), which is already outside the
  host's 19 GiB, so the container itself uses ~100–200 MiB RAM. The GGUF is
  the Ollama blob `sha256-3d0b7905…` read-only; nothing new was downloaded.
  Sampling defaults copy the model's Ollama params (temp 0.6, top_k 20,
  top_p 0.95, min_p 0); the host-RAM prompt cache is capped at 512 MiB.
- `ollama`: Ollama 0.13.5, now **embeddings only** (`nomic-embed-text`, CPU,
  2 threads). It stays on Ollama on purpose: llama.cpp's nomic vectors for
  the same GGUF had cosine only 0.82–0.85 with Ollama's, which would silently
  break every RAG index built through Ollama (local or home-server).
- `gateway`: unchanged contract and keys; chat now goes to llama-server's
  OpenAI API and **SSE is real incremental streaming** (chunks are relayed as
  generated, fields whitelisted).

Limits: chat 1536 MiB / 1.20 CPU, ollama 1024 MiB / 2.50 CPU, gateway
256 MiB / 0.25 CPU — 2.75 GiB RAM total, below the previous 4 GiB, no swap,
3.95 CPUs. Threads ≤ quota: 0 throttled periods measured (Ollama had 89%).

Measured via the LAN gateway (single runs, not an SLA):

| | Ollama CPU, before | llama-server Vulkan, now |
|---|--:|--:|
| 5434-token prompt, time to first token | ~200 s (4596 tok) | **10.9 s** |
| same prompt again (prefix cached) | — | 0.10 s |
| decode, qwen3:1.7b | not measured (0.6B: 27 tok/s) | **~35 tok/s** |
| short chat (27 + 10 tokens) | — | 0.51 s |
| app CLI with full AGENTS.md, 5 invariant retries | ~10 min | 5 s |

The 5434-token prompt recalled the code placed at its start. A request over
8192 tokens now returns a clear **400** (`request (9434 tokens) exceeds the
available context size`) instead of Ollama's silent truncation.

`qwen3:4b` A/B (temporary container, then removed; its blob stays in
`models`, remove with `ollama rm qwen3:4b` if unwanted): 3.8 of 4.0 GB VRAM,
decode 13.4 tok/s, 26 s for the same 5.4k prompt, better answers on the full
project prompt but still failed the invariants accounting, and it was
OOM-killed at 1.5 GiB by the then-uncapped prompt cache. Not adopted.

Rollback: `backups/2026-10-11-pre-vulkan/` on the server holds the previous
`compose.yaml`, `gateway.py`, `control.sh`, `README.md`; copy them back and
run `control.sh start` (the `chat` container then becomes an orphan: stop it
with `docker compose … rm -sf chat`). Models and keys are untouched.

Everything below up to "Install" is the history of the CPU-only setup.

## Inventory and resource budget

Pre-deployment measurement: Ryzen 6800U, 16 host threads; RAM 19 GiB total,
8.0 GiB available; root filesystem 74 GiB available; thin pool 123,078,412 KiB
available. Existing CT100 Syncthing, CT102 media/GPU, stopped CT101 scheduler,
running VM103, and the Multica postgres/backend/frontend/Telegram containers
are not reclaimed or modified.

Hard aggregate limits: **4 GiB RAM, no swap, 3.95 CPUs** (below the 4 CPU
ceiling). Ollama gets 3840 MiB and 3.70 CPUs; gateway gets 256 MiB and 0.25 CPU.
The CPU margin avoids Compose nanocpu rounding exceeding the ceiling.
No privileged mode, device mounts,
GPU reservations or extra capabilities. Both containers run unprivileged with
read-only root filesystems, bounded tmpfs/logs/PIDs and no-new-privileges.
Models persist in `/opt/ask-home-engine/models`; secrets persist separately in
`/opt/ask-home-engine/secrets.env` (0600), never in the repository.

Initial deployment measurement (4096-token window) with Qwen loaded: Ollama 1.933 GiB, gateway
19.93 MiB; host 6.3 GiB available, root 67 GiB available, model files 1.6 GiB.
`ollama ps` reports one `qwen3:1.7b` runner, **100% CPU**, **4096 context**.
Qwen is 2.0B parameters, Q4_K_M; nomic is 137M, F16, 768-dimensional output.
Resolved image digests: Ollama
`sha256:2c9595c555fd70a28363489ac03bd5bf9e7c5bdf2890373c3a830ffd7252ce6d`,
Python `sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641`.

On 2026-10-11 the Qwen window was increased to **8192 tokens** in both
Compose and the chat gateway; the app catalog/footer uses the same limit.
Ollama reported **2.4 GB, 100% CPU, context 8192**; container usage was
2.333 GiB plus 17.78 MiB for the gateway, inside the unchanged 4 GiB budget.
A real authenticated request used **5504 prompt + 9 completion tokens** and
returned the exact code `ORBIT-PINE-732` placed before the filler text,
proving retention beyond the previous 4096-token window. The old 180-second
backend deadline timed out on long CPU prefill; it is now 600 seconds.
Embedding context remains 4096. Longer histories still require context
management; 8192 includes both input and generated output.
A second uncached request used **4596 + 10 tokens**, recalled
`CEDAR-MOON-846` from the beginning, and completed in **203.8 seconds**.
The rebuilt app was launched in a guest PTY: footer
`ctx 435~/8192 (5%)`, `/quit` exit 0; 15 config tests passed.

Both models now remain resident together: `OLLAMA_MAX_LOADED_MODELS=2`,
`OLLAMA_KEEP_ALIVE=-1`, and both gateway request types send `keep_alive=-1`.
This prevents embedding requests from evicting Qwen or idle expiry from
unloading either model; restarting/stopping the engine still unloads them.
On 2026-10-11, authenticated chat → embedding → chat returned **12**,
two **768-dimensional** vectors, then **14**. `ollama ps` showed both
Qwen (**8192** context, **2.4 GB**) and nomic (**4096** context, **565 MB**),
100% CPU, expiry **Forever**. Combined Ollama usage was **2.57 GiB**,
gateway **17.79 MiB**, host available RAM **6.1 GiB**; the aggregate
4 GiB/no-swap cap remains unchanged.

### 2026-10-11: 0.6B trial and initial-prompt measurements

At that point the gateway and app used `qwen3:0.6b`, Q4_K_M, 8192 context.
The earlier measurements above describe 1.7B. Its files remain available,
but its runner is unloaded; Qwen 0.6B and nomic are both CPU/Forever.
Final `ollama ps`: Qwen 1.5 GB, nomic 565 MB. Ollama 1.866 GiB,
gateway 21.18 MiB, host available 6.8 GiB. Memory/no-swap and CPU caps
are unchanged; no unrelated workloads were stopped.

The actual initial app CLI prompt was 4666 tokens: project instruction
file 15,417 characters plus invariant rules and a short Russian question.
0.6B took 115.6 s; 1.7B on the same prompt took 164.9 s, including
162.6 s prefill. Removing only the instruction file kept invariants,
reduced the prompt to 648 tokens and completed in 5.16 s on 0.6B.
The app README documents `/context off`, `/mcp off`, their tradeoffs,
and the observed strict-contract failures. Defaults remain unchanged.
Single measurements, not a latency SLA or a RAG/tool-quality qualification.

The real guest TUI explicit plain-chat control (invariants off, context
off, MCP off) returned `7 + 5 = 12.` with 96/10 tokens in 0.52 s, exit 0.
App `--verify-login` confirmed both scoped APIs with the new model;
embedding still returned 768-dimensional vectors.


## Install (new deployment only)

Docker/Compose, Python 3, SSH key access and at least 4 GiB available RAM are
required. The script refuses to overwrite an existing directory. New source
files need no backup; before modifying any existing remote file, make a dated
private backup and review changes. Never delete models, secrets or other data.

```sh
./deploy.sh /home/shmon/.local/share/kanban4ai/projects/homework/.kanban/backups/TASK-118/home-server-keys.txt
```

This pulls the pinned llama.cpp Vulkan, Ollama 0.13.5 and Python 3.13 images,
then `qwen3:1.7b` (only for its GGUF blob, loaded by `chat`) and
`nomic-embed-text`, starts only this project, and writes a private 0600 bundle.
If Ollama ever re-pulls `qwen3:1.7b` with a different blob digest, update
the `-m` path in `compose.yaml`.
Image tags are explicit; Docker stores the resolved image digests at deploy.
Only Ollama temporarily joins the existing default bridge for model downloads;
`control.sh pull` disconnects it afterward. Its API has **no published port**.
The running backend network is internal. The gateway alone has a LAN-facing
network and publishes **192.168.0.128:11435**, never 0.0.0.0 or IPv6.

## Start / stop / status

```sh
ssh -o BatchMode=yes -o ConnectTimeout=8 root@192.168.0.128 '/opt/ask-home-engine/control.sh start'
ssh -o BatchMode=yes -o ConnectTimeout=8 root@192.168.0.128 '/opt/ask-home-engine/control.sh stop'
ssh -o BatchMode=yes -o ConnectTimeout=8 root@192.168.0.128 '/opt/ask-home-engine/control.sh status'
```

Start checks 4 GiB available when stopped and waits for container health.
Stop preserves models and keys; status prints container health, measured
RAM/CPU usage, hard caps, GPU/device absence and port bindings, never secrets.
Gateway handles SIGTERM explicitly and exits cleanly rather than waiting for
Docker's forced-kill timeout. Stop cancels any active request; it does not delete
models, keys or user data.
`control.sh pull` updates the two allowed local model tags with temporary
egress; it is not needed at every start. Never use `down -v`, Docker prune,
VM/CT stop/destroy, host restart or storage reclamation for this project.

## Private key retrieval

```sh
umask 077
ssh -o BatchMode=yes -o ConnectTimeout=8 root@192.168.0.128 '/opt/ask-home-engine/control.sh keys' > /private/path/home-server-keys.txt
chmod 0600 /private/path/home-server-keys.txt
```

The file contains `LLM_KEY:EMBED_KEY`; each independent key is 64 random hex
characters. Use the individual key for its API, never the bundle as a Bearer.
Do not display keys in terminal logs, chat, commit them, pass their values in
command arguments, or store them in repo files. Docker root access can inspect
environment secrets; this does not protect against a compromised root host.

## HTTP contracts

- `GET http://192.168.0.128:11435/v1/models`: LLM Bearer only; lists
  `qwen3:1.7b`.
- `POST /v1/chat/completions`: LLM Bearer only; OpenAI-compatible plain JSON or
  incremental SSE (`stream: true`) with text `system`, `user`, `assistant`, `tool`
  messages. Model must be exactly `qwen3:1.7b`; images and unknown fields are
  rejected. Supports `temperature`, `top_p`, native `top_k` (-1..2147483647;
  -1 disables vocabulary filtering), `seed`,
  `stop` and either `max_tokens` or `max_completion_tokens` (1..2048; default
  1024). Maps to llama-server `/v1/chat/completions` with
  `enable_thinking=false`, 8192 context, on the iGPU; `top_k` ≤ 0 disables it. Returns actual prompt/completion/total token counts and
  `stop`/`length`/`tool_calls` finish reason.
  Function `tools` (up to 64) and `tool_choice: auto|none` are passed to
  llama-server's Qwen3 template in OpenAI form; tool replies must reference
  an earlier call ID; returned arguments are checked to be JSON objects.
  Forced/specific tool choices are explicitly unsupported.
  `response_format` supports `text`, `json_object`, and `json_schema` via llama-server
  grammar-constrained output. Thinking/reasoning, penalties and other
  cloud-only knobs are rejected, not silently ignored.
  SSE relays backend chunks as they are generated (role, content/tool-call
  deltas, finish, `[DONE]`); `stream_options: {"include_usage":true}` adds the
  final empty-choices usage chunk. Errors before the first byte are ordinary
  HTTP errors; a failure mid-stream is sent as one `{"error":…}` event.
- `POST /api/embed`: embedding Bearer only; Ollama-compatible
  `{"model":"nomic-embed-text","input":["text"]}`. Input may also be a
  single string. At most 64 texts, each 65,536 characters; `truncate` defaults
  false, may be explicitly true. Context remains 4096. Embeddings and timing/
  token fields are returned from the real backend without synthetic values.
- Missing, invalid or cross-scope keys: **401**, including `/v1/models`.
- Unknown model/options: **400**; unknown endpoint **404**; wrong method **405**;
  missing length **411**, oversized body **413**, non-JSON content type **415**.
  Upstream errors preserve their status and meaningful message. Backend timeout
  is **504**, unavailable/malformed response **502**.

Requests are capped at 1 MiB, backend replies at 8 MiB; socket idle timeouts
are 15 seconds for clients and 600 seconds for the backend. Maximum eight HTTP workers;
**one inference request total** across chat and embeddings. A stream holds
the slot until its last chunk. Excess concurrency gets **429** with
Retry-After instead of an unbounded queue. Both models stay resident
(chat in VRAM, nomic in Ollama with keep-alive Forever). No raw Ollama management API is
exposed. Gateway access logs are disabled to avoid logging prompts or keys.

HTTP is deliberately restricted to a trusted home LAN, not encrypted. Do not
configure port forwarding or expose 11435 to the Internet. Use VPN/SSH tunnel
or separately configured HTTPS for remote access. LAN IP binding is not a
substitute for router/firewall policy if a router is later reconfigured.

## Verification responsibility

The coordinator performs the final smoke against the running API and actual TUI:
real chat with positive backend usage, real 768-dimensional embedding,
unauthenticated/wrong/cross-scope rejection, fixed model/context/thinking
behavior, concurrency/body limits, SSE, native tools and start-stop-start.
Container health/status are deployment evidence, not proof of generation.
