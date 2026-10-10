# CPU-only home engine

Isolated Docker Compose project `ask-home-engine`, installed only into the new
`/opt/ask-home-engine` directory on `root@192.168.0.128`. No VM, CT, existing
Docker workload, GPU, host service or router configuration is changed.

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

Post-deployment measurement with Qwen loaded: Ollama 1.933 GiB, gateway
19.93 MiB; host 6.3 GiB available, root 67 GiB available, model files 1.6 GiB.
`ollama ps` reports one `qwen3:1.7b` runner, **100% CPU**, **4096 context**.
Qwen is 2.0B parameters, Q4_K_M; nomic is 137M, F16, 768-dimensional output.
Resolved image digests: Ollama
`sha256:2c9595c555fd70a28363489ac03bd5bf9e7c5bdf2890373c3a830ffd7252ce6d`,
Python `sha256:a1165e272e578941b84abc79e4ab38a0305cd12803a5c4247979ac7655f4d641`.

## Install (new deployment only)

Docker/Compose, Python 3, SSH key access and at least 4 GiB available RAM are
required. The script refuses to overwrite an existing directory. New source
files need no backup; before modifying any existing remote file, make a dated
private backup and review changes. Never delete models, secrets or other data.

```sh
./deploy.sh /home/shmon/.local/share/kanban4ai/projects/homework/.kanban/backups/TASK-118/home-server-keys.txt
```

This pulls pinned Ollama 0.13.5 and Python 3.13 images, then `qwen3:1.7b` and
`nomic-embed-text`, starts only this project, and writes a private 0600 bundle.
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
  buffered SSE (`stream: true`) with text `system`, `user`, `assistant`, `tool`
  messages. Model must be exactly `qwen3:1.7b`; images and unknown fields are
  rejected. Supports `temperature`, `top_p`, native `top_k` (-1..2147483647;
  -1 disables vocabulary filtering), `seed`,
  `stop` and either `max_tokens` or `max_completion_tokens` (1..2048; default
  1024). Maps to Ollama `/api/chat` with **think=false**, **num_ctx=4096**,
  4 CPU threads. Returns actual prompt/completion/total token counts and
  `stop`/`length`/`tool_calls` finish reason.
  Function `tools` (up to 64) and `tool_choice: auto|none` use native Ollama
  tools. Assistant call IDs and JSON-string arguments are converted to native
  function argument objects; tool replies must reference an earlier call and
  are mapped to its function name. Returned calls get unique OpenAI IDs.
  Forced/specific tool choices are explicitly unsupported.
  `response_format` supports `text`, `json_object`, and `json_schema` via native
  Ollama format-constrained output. Thinking/reasoning, penalties and other
  cloud-only knobs are rejected, not silently ignored.
  SSE buffers the real backend response before sending role, content/tool,
  finish deltas and `[DONE]`; it is compatible transport, **not incremental
  token latency**. `stream_options: {"include_usage":true}` adds the final
  empty-choices usage chunk. Backend errors remain ordinary HTTP errors.
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
are 15 seconds for clients and 180 seconds for the backend. Maximum eight HTTP workers;
**one inference request total** across chat and embeddings. Excess concurrency
gets **429** with Retry-After instead of an unbounded queue. Ollama keeps only
one loaded model, one parallel request and a five-minute keep-alive. Switching
between chat and embeddings may reload models. No raw Ollama management API is
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
