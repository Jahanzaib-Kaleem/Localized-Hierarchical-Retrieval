#!/bin/sh
set -eu

CONTAINER_NAME="${LHR_CONTAINER_NAME:-lhr}"
IMAGE="${LHR_IMAGE:-ghcr.io/jahanzaib-kaleem/lhr:latest}"
DATA_VOLUME="${LHR_DATA_VOLUME:-lhr-data}"
STUDIO_PORT="${LHR_STUDIO_PORT:-8787}"
MCP_PORT="${LHR_MCP_PORT:-8788}"
REPLACE="${LHR_REPLACE:-0}"

command -v docker >/dev/null 2>&1 || { echo "Docker was not found in PATH." >&2; exit 127; }

case "$STUDIO_PORT:$MCP_PORT" in
  *[!0-9:]* ) echo "LHR_STUDIO_PORT and LHR_MCP_PORT must be numeric." >&2; exit 64 ;;
esac

if docker ps -a --filter "name=^/${CONTAINER_NAME}$" --format '{{.Names}}' | grep -qx "$CONTAINER_NAME"; then
  if [ "$REPLACE" != "1" ]; then
    echo "Container '$CONTAINER_NAME' already exists. Set LHR_REPLACE=1 to recreate it while preserving the data volume." >&2
    exit 65
  fi
  docker rm -f "$CONTAINER_NAME" >/dev/null
fi

echo "Pulling $IMAGE ..."
docker pull "$IMAGE"

while :; do
  printf '%s' "Choose an LHR administrator access secret (16+ chars; letters, digits, . _ -): " >&2
  stty -echo
  IFS= read -r SECRET
  stty echo
  printf '\n' >&2
  printf '%s' "Confirm the access secret: " >&2
  stty -echo
  IFS= read -r CONFIRM
  stty echo
  printf '\n' >&2
  [ "$SECRET" = "$CONFIRM" ] || { echo "Secrets did not match." >&2; continue; }
  [ "${#SECRET}" -ge 16 ] || { echo "Use at least 16 characters." >&2; continue; }
  case "$SECRET" in *[!A-Za-z0-9._-]*) echo "Only letters, digits, dot, underscore and hyphen are accepted." >&2; continue ;; esac
  break
done

printf '%s' "$SECRET" | docker run --rm -i --entrypoint sh -v "$DATA_VOLUME:/data" "$IMAGE" -c 'umask 077; cat > /data/.lhr-admin-token; chmod 600 /data/.lhr-admin-token'
unset SECRET CONFIRM

echo "Starting LHR ..."
docker run -d \
  --name "$CONTAINER_NAME" \
  --restart unless-stopped \
  -p "127.0.0.1:$STUDIO_PORT:8787" \
  -p "127.0.0.1:$MCP_PORT:8788" \
  -v "$DATA_VOLUME:/data" \
  "$IMAGE"

printf '\nLHR is running.\n'
printf 'Studio: http://127.0.0.1:%s\n' "$STUDIO_PORT"
printf 'MCP:    http://127.0.0.1:%s/mcp\n' "$MCP_PORT"
printf 'Data:   Docker volume %s\n\n' "$DATA_VOLUME"
printf '%s\n' "Studio will ask for the administrator access secret you just chose."
printf '%s\n' "For a public deployment, keep the Docker ports bound to 127.0.0.1 and put HTTPS/private transport in front of them. Do not expose the cleartext listeners directly to the Internet."
