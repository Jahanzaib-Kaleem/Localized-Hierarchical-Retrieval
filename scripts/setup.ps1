param(
    [string]$ContainerName = "lhr",
    [string]$Image = "ghcr.io/jahanzaib-kaleem/lhr:latest",
    [string]$DataVolume = "lhr-data",
    [ValidateRange(1, 65535)][int]$StudioPort = 8787,
    [ValidateRange(1, 65535)][int]$McpPort = 8788,
    [switch]$Replace,
    [switch]$RotateSecret
)

$ErrorActionPreference = "Stop"

function ConvertFrom-LhrSecureString([Security.SecureString]$Value) {
    $ptr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($Value)
    try { return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr) }
}

function Read-LhrAdminSecret {
    while ($true) {
        $firstSecure = Read-Host "Choose an LHR administrator access secret (16+ chars; letters, digits, . _ -)" -AsSecureString
        $secondSecure = Read-Host "Confirm the access secret" -AsSecureString
        $first = ConvertFrom-LhrSecureString $firstSecure
        $second = ConvertFrom-LhrSecureString $secondSecure
        if ($first -ne $second) {
            Write-Host "Secrets did not match." -ForegroundColor Red
            continue
        }
        if ($first.Length -lt 16) {
            Write-Host "Use at least 16 characters." -ForegroundColor Red
            continue
        }
        if ($first -notmatch '^[A-Za-z0-9._-]+$') {
            Write-Host "Only letters, digits, dot, underscore and hyphen are accepted." -ForegroundColor Red
            continue
        }
        return $first
    }
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw "Docker was not found in PATH. Install/start Docker first."
}

$existing = $false
$oldImageId = $null
$existingName = docker ps -a --filter "name=^/$ContainerName$" --format '{{.Names}}'
if ($existingName) {
    $existing = $true
    $inspect = (docker inspect $ContainerName | ConvertFrom-Json)[0]
    $oldImageId = $inspect.Image

    if (-not $PSBoundParameters.ContainsKey("DataVolume")) {
        $mount = $inspect.Mounts | Where-Object { $_.Destination -eq "/data" } | Select-Object -First 1
        if ($mount) {
            if ($mount.Type -eq "volume") { $DataVolume = $mount.Name }
            elseif ($mount.Source) { $DataVolume = $mount.Source }
        }
    }

    if (-not $PSBoundParameters.ContainsKey("StudioPort")) {
        $binding = $inspect.HostConfig.PortBindings.'8787/tcp' | Select-Object -First 1
        if ($binding -and $binding.HostPort) { $StudioPort = [int]$binding.HostPort }
    }

    if (-not $PSBoundParameters.ContainsKey("McpPort")) {
        $binding = $inspect.HostConfig.PortBindings.'8788/tcp' | Select-Object -First 1
        if ($binding -and $binding.HostPort) { $McpPort = [int]$binding.HostPort }
    }

    Write-Host "Existing LHR installation detected. Upgrading in place while preserving '$DataVolume' ..." -ForegroundColor Cyan
} else {
    Write-Host "New LHR installation. Persistent data will use '$DataVolume'." -ForegroundColor Cyan
}

# Pull before removing the current container. Registry/network failure therefore cannot take a
# healthy installation offline.
Write-Host "Pulling $Image ..."
docker pull $Image | Out-Host
if ($LASTEXITCODE -ne 0) { throw "Failed to pull $Image. Existing LHR installation was left untouched." }

$null = docker run --rm --entrypoint sh -v "${DataVolume}:/data" $Image -c 'test -s /data/.lhr-admin-token'
$tokenExists = ($LASTEXITCODE -eq 0)

if ($RotateSecret -or -not $tokenExists) {
    $secret = Read-LhrAdminSecret
    try {
        $secret | docker run --rm -i --entrypoint sh -v "${DataVolume}:/data" $Image -c 'umask 077; tr -d "\r\n" > /data/.lhr-admin-token; chmod 600 /data/.lhr-admin-token'
        if ($LASTEXITCODE -ne 0) { throw "Failed to seed the LHR administrator secret." }
    }
    finally {
        $secret = $null
    }
    if ($tokenExists) { Write-Host "Administrator access secret rotated." -ForegroundColor Yellow }
} else {
    Write-Host "Reusing the existing administrator access secret from the persistent data volume."
}

function Start-LhrContainer([string]$RunImage) {
    $containerId = docker run -d `
        --name $ContainerName `
        --restart unless-stopped `
        -p "127.0.0.1:${StudioPort}:8787" `
        -p "127.0.0.1:${McpPort}:8788" `
        -v "${DataVolume}:/data" `
        $RunImage
    if ($LASTEXITCODE -ne 0) { throw "Failed to create LHR container from $RunImage." }
    return $containerId
}

function Restore-LhrPreviousImage([string]$Reason) {
    docker rm -f $ContainerName 2>$null | Out-Null
    if ($existing -and $oldImageId) {
        Write-Warning "New LHR image did not start cleanly. Restoring the previous image without touching data ..."
        try {
            Start-LhrContainer $oldImageId | Out-Null
            Write-Warning "Previous LHR image restored. Persistent data was not modified by the installer."
        } catch {
            Write-Warning "Automatic container rollback failed. The persistent data mount '$DataVolume' is still intact."
        }
    } else {
        Write-Warning "The persistent data mount '$DataVolume' is still intact."
    }
    throw $Reason
}

if ($existing) {
    Write-Host "Replacing the application container; persistent data is not removed ..."
    docker rm -f $ContainerName | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Could not remove the old LHR container. Persistent data was not removed." }
}

Write-Host "Starting LHR ..."
try {
    Start-LhrContainer $Image | Out-Null
} catch {
    Restore-LhrPreviousImage $_.Exception.Message
}

Start-Sleep -Seconds 2
$running = docker inspect -f '{{.State.Running}}' $ContainerName 2>$null
if ($running -ne "true") {
    Restore-LhrPreviousImage "The new LHR container exited during startup."
}

# Health is a readiness signal, not a data-safety dependency. A very large catalog may take longer
# than this short convenience wait to become ready, so a still-running container is not rolled back
# solely because the HTTP readiness check has not completed yet.
$health = "http://127.0.0.1:${StudioPort}/healthz"
$healthy = $false
for ($attempt = 0; $attempt -lt 60; $attempt++) {
    try {
        $response = Invoke-RestMethod -Uri $health -Method Get -TimeoutSec 2
        if ($response.status -eq "ok") {
            $healthy = $true
            break
        }
    } catch {
        Start-Sleep -Milliseconds 500
    }
}

Write-Host ""
if ($existing) { Write-Host "LHR is upgraded." -ForegroundColor Green }
else { Write-Host "LHR is installed." -ForegroundColor Green }
Write-Host "Studio: http://127.0.0.1:${StudioPort}"
Write-Host "MCP:    http://127.0.0.1:${McpPort}/mcp"
Write-Host "Data:   '$DataVolume' mounted at /data"
if (-not $healthy) {
    Write-Host "The container is running but readiness is still warming up. Check 'docker logs $ContainerName' if it does not become ready." -ForegroundColor Yellow
}
Write-Host ""
Write-Host "Run this same setup command again later to upgrade to the newest image."
Write-Host "Existing database files and the administrator access secret remain in the persistent /data mount."
Write-Host "Use -RotateSecret only when you intentionally want to replace the administrator access secret."
Write-Host "For a public deployment, keep these Docker ports on 127.0.0.1 and put HTTPS/private transport in front of them. Do not expose the cleartext listeners directly to the Internet." -ForegroundColor Yellow
