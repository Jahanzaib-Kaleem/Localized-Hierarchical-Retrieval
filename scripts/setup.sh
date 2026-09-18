#!/bin/sh
set -eu

CONTAINER_NAME="${LHR_CONTAINER_NAME:-lhr}"
IMAGE="${LHR_IMAGE:-ghcr.io/jahanzaib-kaleem/lhr:latest}"
DATA_MOUNT="${LHR_DATA_VOLUME:-lhr-data}"
STUDIO_PORT="${LHR_STUDIO_PORT:-8787}"
MCP_PORT="${LHR_MCP_PORT:-8788}"
ROTATE_SECRET="${LHR_ROTATE_SECRET:-0}"

DATA_MOUNT_EXPLICIT=0
STUDIO_PORT_EXPLICIT=0
MCP_PORT_EXPLICIT=0
[ "${LHR_DATA_VOLUME+x}" = x ] && DATA_MOUNT_EXPLICIT=1
[ "${LHR_STUDIO_PORT+x}" = x ] && STUDIO_PORT_EXPLICIT=1
[ "${LHR_MCP_PORT+x}" = x ] && MCP_PORT_EXPLICIT=1

command -v docker >/dev/null 2>&1 || { echo "Docker was not found in PATH." >&2; exit 127; }

EXISTING=0
OLD_IMAGE_ID=""
if docker ps -a --filter "name=^/${CONTAINER_NAME}$" --format '{{.Names}}' | grep -qx "$CONTAINER_NAME"; then
  EXISTING=1
  OLD_IMAGE_ID="$(docker inspect -f '{{.Image}}' "$CONTAINER_NAME")"

  if [ "$DATA_MOUNT_EXPLICIT" -eq 0 ]; then
    DISCOVERED_MOUNT="$(docker inspect -f '{{range .Mounts}}{{if eq .Destination "/data"}}{{if eq .Type "volume"}}{{.Name}}{{else}}{{.Source}}{{end}}{{end}}{{end}}' "$CONTAINER_NAME")"
    [ -n "$DISCOVERED_MOUNT" ] && DATA_MOUNT="$DISCOVERED_MOUNT"
  fi

  if [ "$STUDIO_PORT_EXPLICIT" -eq 0 ]; then
    DISCOVERED_STUDIO_PORT="$(docker inspect -f '{{with index .HostConfig.PortBindings "8787/tcp"}}{{(index . 0).HostPort}}{{end}}' "$CONTAINER_NAME" 2>/dev/null || true)"
    case "$DISCOVERED_STUDIO_PORT" in *[!0-9]*|'') ;; *) STUDIO_PORT="$DISCOVERED_STUDIO_PORT" ;; esac
  fi

  if [ "$MCP_PORT_EXPLICIT" -eq 0 ]; then
    DISCOVERED_MCP_PORT="$(docker inspect -f '{{with index .HostConfig.PortBindings "8788/tcp"}}{{(index . 0).HostPort}}{{end}}' "$CONTAINER_NAME" 2>/dev/null || true)"
    case "$DISCOVERED_MCP_PORT" in *[!0-9]*|'') ;; *) MCP_PORT="$DISCOVERED_MCP_PORT" ;; esac
  fi
fi

case "$STUDIO_PORT:$MCP_PORT" in
  *[!0-9:]* ) echo "LHR_STUDIO_PORT and LHR_MCP_PORT must be numeric." >&2; exit 64 ;;
esac

if [ "$STUDIO_PORT" -lt 1 ] || [ "$STUDIO_PORT" -gt 65535 ] || [ "$MCP_PORT" -lt 1 ] || [ "$MCP_PORT" -gt 65535 ]; then
  echo "LHR_STUDIO_PORT and LHR_MCP_PORT must be between 1 and 65535." >&2
  exit 64
fi

if [ "$EXISTING" -eq 1 ]; then
  echo "Existing LHR installation detected. Upgrading in place while preserving $DATA_MOUNT ..."
else
  echo "New LHR installation. Persistent data will use $DATA_MOUNT."
fi

# Pull before touching the current container. A failed registry/network operation therefore leaves
# the running installation untouched.
echo "Pulling $IMAGE ..."
docker pull "$IMAGE"

prompt_secret() {
  trap 'stty echo 2>/dev/null || true' EXIT INT TERM
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
  trap - EXIT INT TERM
}

TOKEN_EXISTS=0
if docker run --rm --entrypoint sh -v "$DATA_MOUNT:/data" "$IMAGE" -c 'test -s /data/.lhr-admin-token' >/dev/null 2>&1; then
  TOKEN_EXISTS=1
fi

if [ "$ROTATE_SECRET" = "1" ] || [ "$TOKEN_EXISTS" -eq 0 ]; then
  prompt_secret
  printf '%s' "$SECRET" | docker run --rm -i --entrypoint sh -v "$DATA_MOUNT:/data" "$IMAGE" -c 'umask 077; cat > /data/.lhr-admin-token; chmod 600 /data/.lhr-admin-token'
  unset SECRET CONFIRM
  if [ "$TOKEN_EXISTS" -eq 1 ]; then
    echo "Administrator access secret rotated."
  fi
else
  echo "Reusing the existing administrator access secret from the persistent data volume."
fi

start_lhr() {
  RUN_IMAGE="$1"
  docker run -d \
    --name "$CONTAINER_NAME" \
    --restart unless-stopped \
    -p "127.0.0.1:$STUDIO_PORT:8787" \
    -p "127.0.0.1:$MCP_PORT:8788" \
    -v "$DATA_MOUNT:/data" \
    "$RUN_IMAGE"
}

rollback_upgrade() {
  REASON="$1"
  docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
  if [ "$EXISTING" -eq 1 ] && [ -n "$OLD_IMAGE_ID" ]; then
    echo "New LHR image did not start cleanly. Restoring the previous image without touching data ..." >&2
    if start_lhr "$OLD_IMAGE_ID" >/dev/null 2>&1; then
      echo "Previous LHR image restored. Persistent data was not modified by the installer." >&2
    else
      echo "Automatic container rollback failed. The persistent data mount '$DATA_MOUNT' is still intact." >&2
    fi
  else
    echo "LHR did not start. The persistent data mount '$DATA_MOUNT' is still intact." >&2
  fi
  echo "$REASON" >&2
  exit 1
}

if [ "$EXISTING" -eq 1 ]; then
  echo "Replacing the application container; persistent data is not removed ..."
  docker rm -f "$CONTAINER_NAME" >/dev/null
fi

echo "Starting LHR ..."
if ! start_lhr "$IMAGE" >/dev/null; then
  rollback_upgrade "Failed to create the new LHR container."
fi

# A format/config/startup failure normally terminates the appliance immediately. Keep the old image
# ID until this basic startup gate has passed so an upgrade can roll back the application container.
sleep 2
if [ "$(docker inspect -f '{{.State.Running}}' "$CONTAINER_NAME" 2>/dev/null || true)" != "true" ]; then
  rollback_upgrade "The new LHR container exited during startup."
fi

install_self_update_agent() {
  # Self-update is intentionally a host-side privilege boundary. The LHR container never receives
  # the Docker socket. Admin MCP can only create a narrowly-scoped request file under /data/control;
  # this root-owned systemd helper performs the already-supported safe installer workflow.
  if [ "$(id -u)" -ne 0 ] || ! command -v systemctl >/dev/null 2>&1 || [ ! -d /run/systemd/system ]; then
    echo "MCP self-update was not enabled (root + systemd are required on the host)." >&2
    return 0
  fi

  HOST_DATA_PATH="$(docker inspect -f '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Source}}{{end}}{{end}}' "$CONTAINER_NAME" 2>/dev/null || true)"
  if [ -z "$HOST_DATA_PATH" ] || [ ! -d "$HOST_DATA_PATH" ]; then
    echo "MCP self-update was not enabled (could not resolve the host /data mount)." >&2
    return 0
  fi
  case "$HOST_DATA_PATH" in
    *'
'*|*' '*)
      echo "MCP self-update was not enabled because the host data path contains unsupported whitespace." >&2
      return 0
      ;;
  esac

  CONTROL_DIR="$HOST_DATA_PATH/control"
  install -d -m 0700 -o 10001 -g 10001 "$CONTROL_DIR"

  cat > /usr/local/sbin/lhr-self-update-agent <<'EOF'
#!/bin/sh
set -eu

CONTROL_DIR="${1:?control directory is required}"
CONTAINER_NAME="${2:-lhr}"
REQUEST="$CONTROL_DIR/update-request.json"
STATUS="$CONTROL_DIR/update-status.json"
LOG="$CONTROL_DIR/update.log"
SETUP_URL="https://raw.githubusercontent.com/Jahanzaib-Kaleem/Localized-Hierarchical-Retrieval/main/scripts/setup.sh"

[ -f "$REQUEST" ] || exit 0

write_status() {
  STATE="$1"
  MESSAGE="$2"
  IMAGE_ID="${3:-}"
  NOW="$(date +%s)"
  TMP="$CONTROL_DIR/.update-status-$.tmp"
  printf '{"state":"%s","message":"%s","image_id":"%s","updated_at_epoch":%s}\n' \
    "$STATE" "$MESSAGE" "$IMAGE_ID" "$NOW" > "$TMP"
  chmod 0600 "$TMP"
  mv -f "$TMP" "$STATUS"
  chown 10001:10001 "$STATUS"
}

# Remove the trigger before changing the container so the path unit cannot recursively launch.
rm -f "$REQUEST"
write_status "updating" "Pulling and replacing LHR with the newest published image." ""
: > "$LOG"
chmod 0600 "$LOG"

# Give the MCP HTTP response time to leave the old container before it is replaced.
sleep 2

if LHR_CONTAINER_NAME="$CONTAINER_NAME" sh -c "$(curl -fsSL "$SETUP_URL")" >>"$LOG" 2>&1; then
  IMAGE_ID="$(docker inspect -f '{{.Image}}' "$CONTAINER_NAME" 2>/dev/null || true)"
  write_status "success" "LHR update completed successfully." "$IMAGE_ID"
  exit 0
fi

write_status "failed" "LHR update failed. See update.log; persistent /data was not removed." ""
exit 1
EOF
  chmod 0755 /usr/local/sbin/lhr-self-update-agent

  cat > /etc/systemd/system/lhr-self-update.service <<EOF
[Unit]
Description=LHR host-side self updater
After=docker.service network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/lhr-self-update-agent $CONTROL_DIR $CONTAINER_NAME
EOF

  cat > /etc/systemd/system/lhr-self-update.path <<EOF
[Unit]
Description=Watch for LHR MCP software update requests

[Path]
PathExists=$CONTROL_DIR/update-request.json
Unit=lhr-self-update.service

[Install]
WantedBy=multi-user.target
EOF

  # The marker is read inside the unprivileged container and tells MCP that a host worker exists.
  : > "$CONTROL_DIR/updater-enabled"
  chown 10001:10001 "$CONTROL_DIR/updater-enabled"
  chmod 0600 "$CONTROL_DIR/updater-enabled"

  systemctl daemon-reload
  systemctl enable --now lhr-self-update.path >/dev/null
  echo "MCP self-update: enabled through host-side systemd watcher."
}

install_self_update_agent

printf '\nLHR is %s.\n' "$([ "$EXISTING" -eq 1 ] && printf 'upgraded' || printf 'installed')"
printf 'Studio: http://127.0.0.1:%s\n' "$STUDIO_PORT"
printf 'MCP:    http://127.0.0.1:%s/mcp\n' "$MCP_PORT"
printf 'Data:   %s mounted at /data\n\n' "$DATA_MOUNT"
printf '%s\n' "Run this same setup command again later to upgrade to the newest image."
printf '%s\n' "Existing database files and the administrator access secret remain in the persistent /data mount."
printf '%s\n' "Set LHR_ROTATE_SECRET=1 only when you intentionally want to replace the administrator access secret."
printf '%s\n' "On systemd Linux hosts installed as root, admin MCP can request future safe in-place upgrades without SSH."
printf '%s\n' "For a public deployment, keep the Docker ports bound to 127.0.0.1 and put HTTPS/private transport in front of them. Do not expose the cleartext listeners directly to the Internet."
