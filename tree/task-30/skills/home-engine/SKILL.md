---
name: home-engine
description: Start, stop or inspect the home-server local LLM and RAG embedding engine on Proxmox 192.168.0.128. Use for запуск домашней модели, остановка home-server inference, home engine status, or retrieval of its login bundle. Omp-only skill.
---

# Home engine — omp only

This skill controls only the `ask-home-engine` Docker Compose project in
`/opt/ask-home-engine` on `root@192.168.0.128`. It must not control any other
container, VM, service, GPU, volume or storage pool.

## Before starting

Read status and available RAM; the host also runs Syncthing, media, Multica
and a test VM. Never reclaim their resources without the user's permission.

```sh
ssh -o BatchMode=yes -o ConnectTimeout=8 root@192.168.0.128 'free -h; /opt/ask-home-engine/control.sh status'
```

If the engine is stopped and less than 4 GiB RAM is available, do not start
it: show the inventory and ask the user what to do. Do not stop unrelated
workloads or delete their data. A stopped CT is not permission to reuse it.

## Start / stop

```sh
ssh -o BatchMode=yes -o ConnectTimeout=8 root@192.168.0.128 '/opt/ask-home-engine/control.sh start'
ssh -o BatchMode=yes -o ConnectTimeout=8 root@192.168.0.128 '/opt/ask-home-engine/control.sh stop'
ssh -o BatchMode=yes -o ConnectTimeout=8 root@192.168.0.128 '/opt/ask-home-engine/control.sh status'
```

`stop` preserves the downloaded models and keys. Never use `down -v`,
Docker prune, `pct destroy`, `qm stop`, or host reboot for this engine.
After start, check status and make an authenticated request before claiming
it is ready. After stop, verify the project's containers are stopped.

## Models and app

- CPU-only Qwen3 1.7B (`qwen3:1.7b`), quantized, 4096-token context.
- `nomic-embed-text` embeddings, compatible with the app's existing model.
- LLM base: `http://192.168.0.128:11435/v1`.
- Embeddings: `http://192.168.0.128:11435/api/embed`.
- Separate Bearer keys for chat and embeddings. The app login input is a
  single bundle `LLM_KEY:EMBED_KEY`; never send that bundle as a Bearer key.
- App: `/login home-server`, choose `qwen3:1.7b` in the model picker;
  `/rag provider home-server` chooses remote embeddings. `/rag provider local`
  restores local Ollama.

Keys live outside the repository on the server, mode 0600. Only retrieve
on explicit user request:

```sh
ssh -o BatchMode=yes -o ConnectTimeout=8 root@192.168.0.128 '/opt/ask-home-engine/control.sh keys'
```

Never include keys in logs, commits, status messages or command arguments.
For automated checks read the secrets into process memory and output only
HTTP status, model IDs, token counts and embedding dimensions. Unauthorized
requests and a key used against the wrong endpoint must be rejected.

HTTP is for the trusted home LAN only: keys and prompts are not encrypted.
Do not expose port 11435 to the Internet. Remote access requires a secure
VPN, SSH tunnel or separately configured HTTPS; do not disable TLS checking.

## Source and installation

Server source and deployment instructions are in `tree/task-30/server/`.
The versioned copy of this skill is `tree/task-30/skills/home-engine/`.
Install only into `~/.omp/agent/skills/home-engine/`; do not copy to shared
`~/.agents/skills`, Claude skills or other agents' discovery directories.
