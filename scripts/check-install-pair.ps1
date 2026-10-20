# docs/install.ps1 lands remuda.exe and remuda-session-host.exe as one pair and
# keeps the pair it replaced as *.prev for rollback. An asset from before the
# pair (no host) must not leave a host from another build beside the new daemon.
# Stubbed network and tar, as in check-install-hash.ps1:
#   pwsh -NoProfile -File scripts/check-install-pair.ps1

$ErrorActionPreference = 'Stop'

$script = Join-Path (Split-Path -Parent $PSScriptRoot) 'docs/install.ps1'
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("remuda-check-install-pair-" + [guid]::NewGuid())
$installDir = Join-Path $scratch 'bin'
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

$fakeAsset = Join-Path $scratch 'asset'
Set-Content -Path $fakeAsset -Value 'stands in for the release tarball'
$sha256 = [Security.Cryptography.SHA256]::Create()
$fakeSum = ([BitConverter]::ToString($sha256.ComputeHash([IO.File]::ReadAllBytes($fakeAsset))) -replace '-', '').ToLowerInvariant()
$sha256.Dispose()

function Invoke-WebRequest {
    param([switch]$UseBasicParsing, $Uri, $OutFile)
    if ($Uri -like '*/latest.json') {
        Set-Content -Path $OutFile -Value '{"stable":"9.9.9","nightly":"9.9.9"}'
    } elseif ($Uri -like '*/SHA256SUMS') {
        Set-Content -Path $OutFile -Value "$fakeSum  remuda-9.9.9-x86_64-pc-windows-msvc.tar.gz"
    } else {
        Copy-Item $fakeAsset $OutFile
    }
}
function tar {
    # Called as: tar -xzf <asset> -C <dir>. Unpacks the build named by $global:build.
    Set-Content -Path (Join-Path $args[3] 'remuda.exe') -Value "remuda $global:build"
    if (-not $global:noHost) {
        Set-Content -Path (Join-Path $args[3] 'remuda-session-host.exe') -Value "host $global:build"
    }
    $global:LASTEXITCODE = 0
}

$failures = @()
function Install-Build($name, $build, [switch]$NoHost) {
    $global:build = $build
    $global:noHost = $NoHost
    try {
        & { Get-Content -Raw $script | Invoke-Expression } *>&1 | Out-Null
    } catch {
        $script:failures += "${name}: the install failed: $_"
    }
}
# Asserts FILE in the install dir holds WANT, or is absent when WANT is $null.
function Has($name, $file, $want) {
    $path = Join-Path $installDir $file
    if ($null -eq $want) {
        if (Test-Path $path) { $script:failures += "${name}: $file should be absent" }
    } elseif (-not (Test-Path $path)) {
        $script:failures += "${name}: $file is missing"
    } elseif ((Get-Content -Raw $path).Trim() -ne $want) {
        $script:failures += "${name}: $file holds '$((Get-Content -Raw $path).Trim())', want '$want'"
    }
}

$was = @{}
foreach ($name in 'XDG_DATA_HOME', 'REMUDA_INSTALL_DIR', 'REMUDA_NO_MODIFY_PATH', 'REMUDA_CHANNEL') {
    $was[$name] = [Environment]::GetEnvironmentVariable($name)
}
try {
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    $env:REMUDA_CHANNEL = 'stable'
    $env:REMUDA_INSTALL_DIR = $installDir
    $env:XDG_DATA_HOME = Join-Path $scratch 'data'
    $env:REMUDA_NO_MODIFY_PATH = '1'
    $env:REMUDA_INSTALL_BUTLER = $null

    $n = 'fresh install'
    Install-Build $n 1
    Has $n 'remuda.exe' 'remuda 1'; Has $n 'remuda-session-host.exe' 'host 1'
    Has $n 'remuda.exe.prev' $null; Has $n 'remuda-session-host.exe.prev' $null

    $n = 'upgrade'
    Install-Build $n 2
    Has $n 'remuda.exe' 'remuda 2'; Has $n 'remuda-session-host.exe' 'host 2'
    Has $n 'remuda.exe.prev' 'remuda 1'; Has $n 'remuda-session-host.exe.prev' 'host 1'

    $n = 'release without a host'
    Install-Build $n 3 -NoHost
    Has $n 'remuda.exe' 'remuda 3'; Has $n 'remuda-session-host.exe' $null
    Has $n 'remuda.exe.prev' 'remuda 2'; Has $n 'remuda-session-host.exe.prev' 'host 2'

    $n = 'pair after a hostless release'
    Install-Build $n 4
    Has $n 'remuda.exe' 'remuda 4'; Has $n 'remuda-session-host.exe' 'host 4'
    Has $n 'remuda.exe.prev' 'remuda 3'; Has $n 'remuda-session-host.exe.prev' $null

    $leftover = @(Get-ChildItem -Force -Path $installDir -Filter '.*' -File)
    if ($leftover) { $failures += "staging files left behind: $($leftover.Name -join ', ')" }
} finally {
    foreach ($name in $was.Keys) { [Environment]::SetEnvironmentVariable($name, $was[$name]) }
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $scratch
}

if ($failures) {
    Write-Host "install.ps1: wrong pair handling"
    $failures | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "ok - docs/install.ps1 lands the remuda/session-host pair and keeps the previous pair as .prev"
