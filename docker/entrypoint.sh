#!/bin/sh
set -eu
umask 077

ROOT="${LHR_ROOT:-/data}"
TOKEN_FILE="$ROOT/.lhr-admin-token"
mkdir -p "$ROOT"

if [ -n "${LHR_API_TOKEN:-}" ]; then
  TOKEN="$LHR_API_TOKEN"
elif [ -f "$TOKEN_FILE" ]; then
  TOKEN="$(cat "$TOKEN_FILE")"
else
  TOKEN="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
  printf '%s\n' "$TOKEN" > "$TOKEN_FILE"
  printf '%s\n' "LHR generated an administrator token for this data volume." >&2
  printf '%s\n' "Retrieve it with: docker exec <container> cat $TOKEN_FILE" >&2
fi

if [ "${#TOKEN}" -lt 16 ]; then
  printf '%s\n' "LHR_API_TOKEN must contain at least 16 characters" >&2
  exit 64
fi
case "$TOKEN" in
  *[!A-Za-z0-9._-]*)
    printf '%s\n' "LHR_API_TOKEN may contain only letters, digits, dot, underscore, and hyphen" >&2
    exit 64
    ;;
esac

cat > /tmp/lhr-service.json <<EOF
{
  "bind": "0.0.0.0:8787",
  "api_keys": [{"id":"studio-admin","token":"$TOKEN","role":"admin"}],
  "behind_tls_proxy": true
}
EOF

exec /usr/local/bin/lhr --root "$ROOT" serve --config /tmp/lhr-service.json
