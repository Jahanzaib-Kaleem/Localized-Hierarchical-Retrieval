param(
    [string]$ContainerName = "lhr",
    [string]$Image = "ghcr.io/jahanzaib-kaleem/lhr:latest",
    [string]$DataVolume = "lhr-data",
    [ValidateRange(1, 65535)][int]$StudioPort = 8787,
    [ValidateRange(1, 65535)][int]$McpPort = 8788,
    [switch]$Replace
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

$existing = docker ps -a --filter "name=^/$ContainerName$" --format '{{.Names}}'
if ($existing) {
    if (-not $Replace) {
        throw "Container '$ContainerName' already exists. Re-run with -Replace to recreate it while preserving the data volume."
    }
    docker rm -f $ContainerName | Out-Null
}

Write-Host "Pulling $Image ..."
docker pull $Image | Out-Host

$secret = Read-LhrAdminSecret
try {
    # Seed the credential directly into the named Docker volume over stdin. The secret is not
    # placed in the docker command line or container environment, and normal container replacement
    # continues to use the same protected volume credential.
    $secret | docker run --rm -i --entrypoint sh -v "${DataVolume}:/data" $Image -c 'umask 077; tr -d "\r\n" > /data/.lhr-admin-token; chmod 600 /data/.lhr-admin-token'
    if ($LASTEXITCODE -ne 0) { throw "Failed to seed the LHR administrator secret." }
}
finally {
    $secret = $null
}

Write-Host "Starting LHR ..."
docker run -d `
    --name $ContainerName `
    --restart unless-stopped `
    -p "127.0.0.1:${StudioPort}:8787" `
    -p "127.0.0.1:${McpPort}:8788" `
    -v "${DataVolume}:/data" `
    $Image | Out-Host

$health = "http://127.0.0.1:${StudioPort}/healthz"
for ($attempt = 0; $attempt -lt 30; $attempt++) {
    try {
        $response = Invoke-RestMethod -Uri $health -Method Get -TimeoutSec 2
        if ($response.status -eq "ok") { break }
    } catch {
        Start-Sleep -Milliseconds 500
    }
}

Write-Host ""
Write-Host "LHR is running." -ForegroundColor Green
Write-Host "Studio: http://127.0.0.1:${StudioPort}"
Write-Host "MCP:    http://127.0.0.1:${McpPort}/mcp"
Write-Host "Data:   Docker volume '$DataVolume'"
Write-Host ""
Write-Host "Studio will ask for the administrator access secret you just chose."
Write-Host "For a public deployment, keep these Docker ports on 127.0.0.1 and put HTTPS/private transport in front of them. Do not expose the cleartext listeners directly to the Internet." -ForegroundColor Yellow
