#!/bin/sh
set -eu
cd "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
compose() { docker compose --project-name ask-home-engine --env-file secrets.env "$@"; }
headroom() {
    available=$(free -b | awk '/^Mem:/ {print $7}')
    if [ "$available" -lt 4294967296 ]; then
        echo 'Refusing start: less than 4 GiB available; do not reclaim other workloads.' >&2
        free -h >&2
        exit 1
    fi
}
case "${1:-status}" in
    start)
        # Running engine memory already belongs to this project, not free RAM.
        if [ -z "$(compose ps --status running -q)" ]; then headroom; fi
        compose up -d --wait --wait-timeout 180
        ;;
    stop)
        compose stop --timeout 30
        compose ps --all
        ;;
    status)
        free -h
        compose ps --all
        ids=$(compose ps -q)
        if [ -n "$ids" ]; then
            docker stats --no-stream $ids
            docker inspect --format '{{.Name}} memory={{.HostConfig.Memory}} swap={{.HostConfig.MemorySwap}} nanoCPUs={{.HostConfig.NanoCpus}} privileged={{.HostConfig.Privileged}} devices={{json .HostConfig.Devices}} ports={{json .NetworkSettings.Ports}}' $ids
        fi
        du -sh models
        ;;
    pull)
        headroom
        compose up -d ollama --wait --wait-timeout 180
        id=$(compose ps -q ollama)
        # Only this new container gets temporary egress; never publish its API.
        docker network connect bridge "$id"
        trap 'docker network disconnect bridge "$id"' EXIT HUP INT TERM
        compose exec -T ollama ollama pull qwen3:1.7b
        compose exec -T ollama ollama pull nomic-embed-text
        compose exec -T ollama ollama list
        ;;
    keys)
        # Explicit private retrieval only. Do not put this output in logs/chat.
        exec python3 -c 'from pathlib import Path; v=dict(line.split("=",1) for line in Path("secrets.env").read_text().splitlines()); print(v["LLM_KEY"]+":"+v["EMBED_KEY"])'
        ;;
    *) echo 'Usage: control.sh start|stop|status|pull|keys' >&2; exit 2 ;;
esac
