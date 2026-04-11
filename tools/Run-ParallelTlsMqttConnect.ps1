param(
    [int]$Count = 3,
    [string]$ServerAddress = "192.168.0.7",
    [int]$ServerPort = 8883,
    [string]$BuildRoot = "$env:USERPROFILE\codex-target\mbedtls-host-build",
    [string]$SerialPortName = "COM4",
    [int]$BaudRate = 115200,
    [int]$HoldMs = 5000,
    [int]$ReadTimeoutMs = 5000,
    [string]$ClientIdPrefix = "parallel-tls-mqtt",
    [switch]$Rebuild
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$mbedtlsRoot = Join-Path $repoRoot "vendor\mbedtls-rs\mbedtls-rs-sys\mbedtls"
$clientProjectRoot = Join-Path $repoRoot "tools\host_tls_mqtt_client"
$clientBuildRoot = Join-Path $BuildRoot "mqtt-client"
$clientExe = Join-Path $clientBuildRoot "Release\tls_mqtt_client.exe"
$tempRoot = Join-Path $env:TEMP ("tls-mqtt-parallel-" + [guid]::NewGuid().ToString("N"))

function Ensure-MbedTlsBuild {
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

function Build-MqttClient {
    New-Item -ItemType Directory -Force -Path $clientBuildRoot | Out-Null

    cmake -S $clientProjectRoot -B $clientBuildRoot `
        -G "Visual Studio 17 2022" `
        -A x64 `
        "-DMBEDTLS_ROOT=$mbedtlsRoot" `
        "-DMBEDTLS_BUILD_ROOT=$BuildRoot" | Out-Host

    cmake --build $clientBuildRoot --config Release --target tls_mqtt_client --parallel 16 | Out-Host
}

if ($Rebuild -or -not (Test-Path (Join-Path $BuildRoot "programs\ssl\Release\ssl_client2.exe"))) {
    Ensure-MbedTlsBuild
}

if ($Rebuild -or -not (Test-Path $clientExe)) {
    Build-MqttClient
}

New-Item -ItemType Directory -Force -Path $tempRoot | Out-Null
$serialLogPath = Join-Path $tempRoot "serial.log"
$serialBuilder = [System.Text.StringBuilder]::new()
$clientRuns = @()
$serial = $null

try {
    $serial = [System.IO.Ports.SerialPort]::new($SerialPortName, $BaudRate, [System.IO.Ports.Parity]::None, 8, [System.IO.Ports.StopBits]::One)
    $serial.ReadTimeout = 100
    $serial.WriteTimeout = 100
    $serial.Open()
    Start-Sleep -Milliseconds 200
    $null = $serial.ReadExisting()

    foreach ($index in 1..$Count) {
        $clientId = "$ClientIdPrefix-$index"
        $stdoutPath = Join-Path $tempRoot "client-$index.stdout.txt"
        $stderrPath = Join-Path $tempRoot "client-$index.stderr.txt"
        $arguments = @(
            "server_addr=$ServerAddress",
            "server_port=$ServerPort",
            "client_id=$clientId",
            "hold_ms=$HoldMs",
            "read_timeout_ms=$ReadTimeoutMs"
        )

        $process = Start-Process `
            -FilePath $clientExe `
            -ArgumentList $arguments `
            -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath `
            -PassThru

        $clientRuns += [pscustomobject]@{
            Index = $index
            ClientId = $clientId
            Process = $process
            StdoutPath = $stdoutPath
            StderrPath = $stderrPath
        }
    }

    while (($clientRuns | Where-Object { -not $_.Process.HasExited }).Count -gt 0) {
        $chunk = $serial.ReadExisting()
        if ($chunk) {
            $null = $serialBuilder.Append($chunk)
        }
        Start-Sleep -Milliseconds 100
    }

    Start-Sleep -Milliseconds 500
    $chunk = $serial.ReadExisting()
    if ($chunk) {
        $null = $serialBuilder.Append($chunk)
    }
}
finally {
    if ($serial -and $serial.IsOpen) {
        $serial.Close()
    }

    $serialText = $serialBuilder.ToString()
    Set-Content -Path $serialLogPath -Value $serialText
}

$allOk = $true

foreach ($run in $clientRuns) {
    $run.Process.WaitForExit()
    $run.Process.Refresh()

    $stdout = ""
    if (Test-Path $run.StdoutPath) {
        $stdout = [string](Get-Content -Path $run.StdoutPath -Raw)
    }

    $stderr = ""
    if (Test-Path $run.StderrPath) {
        $stderr = [string](Get-Content -Path $run.StderrPath -Raw)
    }

    $exitCode = $run.Process.ExitCode
    $exitLabel = if ($null -eq $exitCode) { "?" } else { [string]$exitCode }

    Write-Host ""
    Write-Host "=== client $($run.Index) / $($run.ClientId) / exit=$exitLabel ==="
    if ($stdout) { Write-Host $stdout.TrimEnd() }
    if ($stderr) { Write-Host $stderr.TrimEnd() }

    $accepted = -not [string]::IsNullOrEmpty($stdout) -and $stdout.Contains("MQTT CONNACK accepted for $($run.ClientId)")
    $stderrHasContent = -not [string]::IsNullOrWhiteSpace($stderr)
    if (-not $accepted -or $stderrHasContent) {
        $allOk = $false
    }
}

$serialText = Get-Content -Path $serialLogPath -Raw
$workerMatches = [regex]::Matches($serialText, "\[TLS WORKER (\d+)\] Handshake complete")
$connectMatches = [regex]::Matches($serialText, "\[CONNECT\] accepted session=\d+")
$workerIds = $workerMatches | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique

Write-Host ""
Write-Host "=== serial highlights ($SerialPortName) ==="
if ($workerMatches.Count -gt 0) {
    Write-Host ("TLS handshakes: " + ($workerIds -join ", "))
}
if ($connectMatches.Count -gt 0) {
    Write-Host ("Accepted MQTT sessions: " + $connectMatches.Count)
}

$interesting = $serialText -split "`r?`n" | Where-Object {
    $_ -match "\[ACCEPT/TLS\] Accepted" -or
    $_ -match "\[TLS WORKER \d+\] Handshake complete" -or
    $_ -match "\[CONNECT\] accepted session=" -or
    $_ -match "\[SESSION \d+\] DISCONNECT" -or
    $_ -match "\[TLS WORKER \d+\] Connection closed"
}

if ($interesting.Count -gt 0) {
    $interesting | Select-Object -First 40 | ForEach-Object { Write-Host $_ }
}

if (-not $allOk) {
    throw "At least one parallel MQTT-over-TLS client did not finish successfully."
}

Write-Host ""
Write-Host "Parallel MQTT-over-TLS test succeeded against $ServerAddress`:$ServerPort with $Count clients."
