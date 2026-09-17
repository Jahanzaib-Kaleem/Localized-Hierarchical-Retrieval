# Install and Upgrade

LHR's supported Docker setup is intentionally **idempotent**: the same setup command is used for a first installation and for later upgrades.

The application container is disposable. Durable database state and the administrator credential live under `/data`, normally backed by the named Docker volume `lhr-data`.

## Linux / macOS: one command

```bash
sh -c "$(curl -fsSL https://raw.githubusercontent.com/Jahanzaib-Kaleem/Localized-Hierarchical-Retrieval/main/scripts/setup.sh)"
```

Run that exact command on a new machine to install LHR. Run the exact same command again later to upgrade the existing installation to the newest `ghcr.io/jahanzaib-kaleem/lhr:latest` image.

On the first install the setup asks for an administrator access secret. On later runs it reuses the existing secret stored in `/data/.lhr-admin-token` and does not ask for a new one.

Custom ports can be supplied on first install:

```bash
LHR_STUDIO_PORT=3000 LHR_MCP_PORT=8000 sh -c "$(curl -fsSL https://raw.githubusercontent.com/Jahanzaib-Kaleem/Localized-Hierarchical-Retrieval/main/scripts/setup.sh)"
```

A later upgrade without those variables discovers the existing `/data` mount and published host ports from the current container and preserves them.

To intentionally rotate the administrator credential during a setup/upgrade:

```bash
LHR_ROTATE_SECRET=1 sh -c "$(curl -fsSL https://raw.githubusercontent.com/Jahanzaib-Kaleem/Localized-Hierarchical-Retrieval/main/scripts/setup.sh)"
```

## Windows / PowerShell

From a repository checkout:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\setup.ps1
```

Or directly from the published setup script:

```powershell
$setup = Invoke-RestMethod 'https://raw.githubusercontent.com/Jahanzaib-Kaleem/Localized-Hierarchical-Retrieval/main/scripts/setup.ps1'; & ([scriptblock]::Create($setup))
```

The same command installs when no LHR container exists and upgrades when one already exists.

Use `-RotateSecret` only when credential rotation is intentional.

## What an upgrade does

The setup script performs these operations in order:

1. detect the existing LHR container, if present;
2. discover its current `/data` mount and host ports unless explicit replacements were supplied;
3. record the previous container image ID for application rollback;
4. pull the requested new image **before stopping the current container**;
5. reuse the administrator credential already stored on the persistent `/data` mount;
6. remove only the old application container;
7. create the replacement container against the same `/data` mount;
8. confirm that the replacement remains running;
9. if startup fails immediately, remove the failed replacement and attempt to recreate the previous image against the same persistent data.

The installer never removes the persistent volume.

## Why the database survives

The container image contains the executable, Studio assets, and service runtime. The database catalog is mounted separately at `/data`.

With the normal installation this is:

```text
Docker volume lhr-data -> /data
```

Removing or recreating the `lhr` container therefore does not remove the catalog, generations, deltas, dictionaries, row-ID maps, telemetry, or `.lhr-admin-token` stored in that volume.

Do **not** use destructive volume commands unless you intentionally want to erase the installation. In particular:

```text
docker volume rm lhr-data
docker compose down -v
```

can delete persistent data. Normal install/upgrade does not run either operation.

## Docker Compose

Compose uses the same persistent `lhr-data` volume. The equivalent explicit upgrade is:

```bash
docker compose pull && docker compose up -d
```

Do not add `-v` to `docker compose down` when the intention is only to replace the application container.

## Format compatibility

Container replacement and database migration are deliberately separate concerns.

The current durable format is `LHR/1`. Compatible application releases open the existing catalog directly. A future incompatible format such as `LHR/2` must provide an explicit migration/rebuild path; the installer must not silently reinterpret or destroy an older catalog merely because a newer container image was pulled.

If a replacement image cannot open the existing catalog and exits during startup, the setup script attempts to restore the previous application image while leaving `/data` untouched.

## Upgrade safety rule

**Application containers are replaceable. `/data` is not.**

Any future deployment tooling should preserve that separation.
