# LHR deployment security

LHR has two distinct security boundaries:

1. **server authentication** — the Rust HTTP API and MCP control plane require bearer credentials and enforce roles server-side;
2. **Studio lock screen** — the browser application does not mount the database UI until the server accepts the administrator access secret.

The lock screen is intentionally not the security boundary by itself. Even if somebody downloads the public HTML/JavaScript assets, they cannot read or mutate database data without a valid server credential.

## Default Docker credential

On first start, the container creates a cryptographically random administrator access secret and stores it in the persistent data volume:

```text
/data/.lhr-admin-token
```

The entrypoint uses a restrictive umask and enforces mode `0600` where supported. Retrieve the generated secret locally with:

```bash
docker exec lhr cat /data/.lhr-admin-token
```

Studio keeps the entered credential in `sessionStorage` only. Closing the browser session clears it. The server never sends the administrator secret to the browser.

The MCP listener uses the same role-based bearer-key mechanism. Remote MCP is never intended to be anonymous.

## Interactive setup wizard

### Windows / PowerShell

Choose the Studio host port and MCP host port while keeping both listeners loopback-only:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\setup.ps1 -StudioPort 3000 -McpPort 8000
```

The wizard:

- prompts twice for an administrator access secret;
- validates the secret before starting the appliance;
- pipes the secret over stdin into a temporary copy of the LHR image;
- writes it directly to the persistent `lhr-data` volume;
- does not put the secret in the Docker command line;
- launches Studio/API and MCP bound to `127.0.0.1`;
- preserves the data volume and credential across ordinary container replacement.

Use `-Replace` when intentionally recreating an existing container while keeping the same named volume.

### Linux / macOS

Run:

```bash
sh scripts/setup.sh
```

Optional host-port overrides:

```bash
LHR_STUDIO_PORT=3000 LHR_MCP_PORT=8000 sh scripts/setup.sh
```

## Public deployment

Do **not** publish the raw LHR HTTP listeners directly to the public Internet, even when a strong credential is configured. A password/bearer secret sent over plain HTTP can be intercepted.

Keep Docker bound to loopback:

```text
127.0.0.1:8787 -> Studio/API
127.0.0.1:8788 -> MCP
```

or map any preferred local host ports, for example:

```text
127.0.0.1:3000 -> 8787
127.0.0.1:8000 -> 8788
```

Then terminate HTTPS with a trusted reverse proxy or private tunnel. Only the proxy/tunnel should be Internet-facing.

A minimal Caddy-style shape is:

```text
studio.example.com {
    reverse_proxy 127.0.0.1:3000
}

mcp.example.com {
    reverse_proxy 127.0.0.1:8000
}
```

Use separate hostnames or routing rules if you want different network policies for human Studio access and MCP access.

## Credential rotation

The effective Docker credential is persisted in `/data/.lhr-admin-token`. To rotate it, recreate the container with a newly seeded secret while preserving the database volume. The setup scripts are the preferred interactive path.

The entrypoint also supports:

- `LHR_API_TOKEN` for automation environments;
- `LHR_API_TOKEN_FILE` for orchestrators that mount a secret file.

If either is supplied, the effective credential is persisted into the protected LHR data volume for subsequent ordinary container restarts.

## Roles and least privilege

The underlying service supports:

```text
read < write < admin
```

The one-command appliance uses one administrator credential for simplicity. For shared or externally integrated deployments, define dedicated API keys for clients that only need read or write access. MCP tool discovery and invocation remain role-gated.

For example, a lead-retrieval integration normally only needs `read`; maintenance tools such as compaction, vacuum, recovery and index administration require `admin`.

## What remains intentionally public

`GET /healthz` and `GET /readyz` are intentionally unauthenticated so local orchestrators can test process/dataset readiness. They do not expose rows or credentials.

Static Studio assets may also be downloaded without authentication. They contain application code only, not database content. All database reads, metrics, mutations and administration still pass through authenticated Rust endpoints.
