#!/bin/sh
# Installs only into a NEW directory; refuses to overwrite an existing deployment.
set -eu
cd "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
HOST=root@192.168.0.128
ssh -o BatchMode=yes -o ConnectTimeout=8 "$HOST" 'set -eu; test ! -e /opt/ask-home-engine || { echo "Existing deployment: refusing overwrite; back up and review changes first" >&2; exit 1; }; mkdir -m 0755 /opt/ask-home-engine; mkdir -m 0755 /opt/ask-home-engine/models; chown 1000:1000 /opt/ask-home-engine/models'
scp -o BatchMode=yes -o ConnectTimeout=8 compose.yaml gateway.py control.sh README.md "$HOST":/opt/ask-home-engine/
ssh -o BatchMode=yes -o ConnectTimeout=8 "$HOST" 'set -eu; chmod 0755 /opt/ask-home-engine/control.sh; chmod 0644 /opt/ask-home-engine/compose.yaml /opt/ask-home-engine/gateway.py /opt/ask-home-engine/README.md; python3 -c '\''import os,secrets; p="/opt/ask-home-engine/secrets.env"; fd=os.open(p,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600); os.write(fd,("LLM_KEY="+secrets.token_hex(32)+"\nEMBED_KEY="+secrets.token_hex(32)+"\n").encode()); os.close(fd)'\''; cd /opt/ask-home-engine; docker compose --project-name ask-home-engine --env-file secrets.env pull; ./control.sh pull; ./control.sh start'
if [ "$#" -gt 0 ]; then
    # This is a private file, not a shell argument containing the actual keys.
    umask 077
    mkdir -p -- "$(dirname -- "$1")"
    ssh -o BatchMode=yes -o ConnectTimeout=8 "$HOST" /opt/ask-home-engine/control.sh keys > "$1"
    chmod 0600 "$1"
    echo "Private login bundle saved to $1 (0600); values not printed."
fi
ssh -o BatchMode=yes -o ConnectTimeout=8 "$HOST" /opt/ask-home-engine/control.sh status
