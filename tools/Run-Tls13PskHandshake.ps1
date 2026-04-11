param(
    [string]$ServerAddress = "192.168.0.7",
    [int]$ServerPort = 8883,
    [string]$BuildRoot = "$env:USERPROFILE\codex-target\mbedtls-host-build",
    [int]$ReadTimeoutMs = 1000,
    [switch]$Rebuild
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$mbedtlsRoot = Join-Path $repoRoot "vendor\mbedtls-rs\mbedtls-rs-sys\mbedtls"
$clientExe = Join-Path $BuildRoot "programs\ssl\Release\ssl_client2.exe"
$pskIdentity = "gatomqtt-psk"
$pskHex = "9A3FC152D86B17A4E3297C10B54D8E6213F7A93C58DE71248BC64A9105EF367D"

function Build-Client {
    New-Item -ItemType Directory -Force -Path $BuildRoot | Out-Null

    cmake -S $mbedtlsRoot -B $BuildRoot `
        -G "Visual Studio 17 2022" `
        -A x64 `
        -DUSE_SHARED_MBEDTLS_LIBRARY=OFF `
        -DUSE_STATIC_MBEDTLS_LIBRARY=ON `
        -DENABLE_PROGRAMS=ON `
        -DENABLE_TESTING=OFF `
        -DMBEDTLS_FATAL_WARNINGS=OFF | Out-Host

    cmake --build $BuildRoot --config Release --target ssl_client2 --parallel 16 | Out-Host
}

if ($Rebuild -or -not (Test-Path $clientExe)) {
    Build-Client
}

$output = & $clientExe `
    "server_addr=$ServerAddress" `
    "server_port=$ServerPort" `
    "auth_mode=none" `
    "psk_identity=$pskIdentity" `
    "psk=$pskHex" `
    "min_version=tls13" `
    "max_version=tls13" `
    "tls13_kex_modes=psk" `
    "read_timeout=$ReadTimeoutMs" `
    "skip_close_notify=1" `
    "debug_level=1" 2>&1

$output | Out-Host

$text = ($output | Out-String)
$handshakeOk = $text.Contains("[ Protocol is TLSv1.3 ]") -and $text.Contains("Performing the SSL/TLS handshake...") -and $text.Contains(" ok")

if ($handshakeOk) {
    Write-Host ""
    Write-Host "TLS 1.3 pure PSK handshake succeeded against $ServerAddress`:$ServerPort"
    exit 0
}

Write-Error "TLS 1.3 pure PSK handshake did not complete successfully."
