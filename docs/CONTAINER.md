# LHR container distribution

LHR ships as one container image containing the Rust database/service and the precompiled LHR Studio assets. Node.js exists only in the Docker build stage; it is not present in the runtime image.

## One-command local start

```bash
docker run -d \
  --name lhr \
  --restart unless-stopped \
  -p 127.0.0.1:8787:8787 \
  -p 127.0.0.1:8788:8788 \
  -v lhr-data:/data \
  ghcr.io/jahanzaib-kaleem/lhr:latest
```

Open Studio at `http://127.0.0.1:8787`. MCP is available at `http://127.0.0.1:8788/mcp`.

The container creates one administrator API token the first time a new data volume is used. Retrieve it with:

```bash
docker exec lhr cat /data/.lhr-admin-token
```

Paste that token into **Studio → Settings → API credential**. The credential is kept in browser `sessionStorage`, not persistent browser storage.

A fresh volume intentionally starts with `readyz = not_ready` until at least one bucket has a published dataset. The control plane and Studio still start, so an empty installation remains administrable.

Studio provides a bounded, authenticated streamed-CSV convenience import. Very large/offline imports and arbitrary filesystem-path ingestion remain local CLI operations; LHR does not turn the HTTP API into a general filesystem interface.

## Docker Compose

```bash
docker compose up -d
```

The supplied Compose file binds Studio/API and MCP to host loopback and persists the reserved default catalog, named buckets, credentials, telemetry, and updater control state in the `lhr-data` volume.

## Remote deployment

Do not publish the canonical loopback quickstart directly to the public internet. Put LHR behind TLS and normal network access controls. The container's internal service always requires its generated/admin API token; bearer credentials must not cross an untrusted cleartext network.

Set `LHR_API_TOKEN` to a secret with at least 16 characters if an orchestrator should own the credential instead of the persistent token file. Allowed characters are letters, digits, `.`, `_`, and `-`.

## Studio serving

The runtime image sets `LHR_STUDIO_DIR=/opt/lhr/studio`. Axum serves those compiled assets as the fallback for browser routes while API paths retain their existing `/v1`, `/metrics`, `/healthz`, and `/readyz` contracts. Hashed Vite assets receive immutable cache headers; `index.html` is not cached.

## Image publishing

The `LHR Container` workflow currently publishes:

- `ghcr.io/jahanzaib-kaleem/lhr:latest` from `main`;
- `ghcr.io/jahanzaib-kaleem/lhr:mcp` from `mcp-control-plane`;
- semantic version tags from `v*` refs.

The `mcp-control-plane` build is amd64-only; `main` and version tags publish amd64 + arm64.

The image workflow is path-filtered to runtime/image inputs (`rust/**`, `studio/**`, Docker files, and the workflow itself). Changes confined to `README.md` and `docs/**` do not request a GHCR rebuild.
