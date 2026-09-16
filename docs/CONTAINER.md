# LHR container distribution

LHR ships as one container image containing the Rust database/service and the precompiled LHR Studio assets. Node.js exists only in the Docker build stage; it is not present in the runtime image.

## One-command local start

```bash
docker run -d --name lhr --restart unless-stopped -p 127.0.0.1:8787:8787 -v lhr-data:/data ghcr.io/jahanzaib-kaleem/lhr:latest
```

Open `http://127.0.0.1:8787`.

The container creates one administrator API token the first time a new data volume is used. Retrieve it with:

```bash
docker exec lhr cat /data/.lhr-admin-token
```

Paste that token into **Studio → Settings → API credential**. The credential is kept in browser `sessionStorage`, not persistent browser storage.

A fresh volume intentionally starts with `readyz = not_ready` until an initial dataset is imported. The control plane and Studio still start, so initialization failures do not make the container itself unreachable. Bulk imports remain local operations; mount/import the source and run the LHR CLI against `/data` rather than granting the HTTP API arbitrary filesystem-path access.

## Docker Compose

```bash
docker compose up -d
```

The supplied Compose file binds Studio/API to host loopback and persists the catalog in the `lhr-data` volume.

## Remote deployment

Do not publish the canonical loopback quickstart directly to the public internet. Put LHR behind TLS and normal network access controls. The container's internal service always requires its generated/admin API token; bearer credentials must not cross an untrusted cleartext network.

Set `LHR_API_TOKEN` to a secret with at least 16 characters if an orchestrator should own the credential instead of the persistent token file. Allowed characters are letters, digits, `.`, `_`, and `-`.

## Studio serving

The runtime image sets `LHR_STUDIO_DIR=/opt/lhr/studio`. Axum serves those compiled assets as the fallback for browser routes while API paths retain their existing `/v1`, `/metrics`, `/healthz`, and `/readyz` contracts. Hashed Vite assets receive immutable cache headers; `index.html` is not cached.

## Image publishing

The `LHR Container` workflow publishes `ghcr.io/jahanzaib-kaleem/lhr:studio` from the `lhr-studio` branch and `:latest` from `main`. Development-branch image builds are amd64 only to avoid wasting QEMU minutes; `main` and version tags publish amd64 + arm64.
