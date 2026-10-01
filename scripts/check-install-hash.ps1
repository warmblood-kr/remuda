# Runs docs/install.ps1 against a stubbed network and asserts its checksum
# step does its job: a download that matches SHA256SUMS is installed, one that
# does not is refused.
#
# On its own that is true in any shell. It is here to be run in the shell where
# it was not: Windows PowerShell started underneath PowerShell 7 (which is what
# `remuda upgrade` typed into pwsh does) finds no Get-FileHash, so the installer
# must not need it - and neither may this check, which hashes with .NET.
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check-install-hash.ps1
#   pwsh -NoProfile -File scripts/check-install-hash.ps1

$ErrorActionPreference = 'Stop'

$script = Join-Path (Split-Path -Parent $PSScriptRoot) 'docs/install.ps1'
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("remuda-check-install-hash-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

$fakeAsset = Join-Path $scratch 'asset'
Set-Content -Path $fakeAsset -Value 'stands in for the release tarball'
$sha256 = [Security.Cryptography.SHA256]::Create()
$goodSum = [BitConverter]::ToString($sha256.ComputeHash([IO.File]::ReadAllBytes($fakeAsset))) -replace '-', ''
$sha256.Dispose()
$badSum = '0' * 64

# Functions shadow cmdlets and applications, and `iex` runs the installer in
# a scope that sees them, so these are what it calls instead of the network
# and tar. $publishedSum is what SHA256SUMS will claim.
function Invoke-WebRequest {
    param([switch]$UseBasicParsing, $Uri, $OutFile)
    if ($Uri -like '*/latest.json') {
        Set-Content -Path $OutFile -Value '{"stable":"9.9.9","nightly":"9.9.9"}'
    } elseif ($Uri -like '*/SHA256SUMS') {
        Set-Content -Path $OutFile -Value "$publishedSum  ./remuda-9.9.9-x86_64-pc-windows-msvc.tar.gz"
    } else {
        Copy-Item $fakeAsset $OutFile
    }
}
function tar {
    # Called as: tar -xzf <asset> -C <dir>
    Set-Content -Path (Join-Path $args[3] 'remuda.exe') -Value 'not a real binary'
    $global:LASTEXITCODE = 0
}

# Installs into its own directory; returns the failure message, or $null.
function Install-Into($installDir) {
    $env:REMUDA_INSTALL_DIR = $installDir
    try {
        & { Get-Content -Raw $script | Invoke-Expression } 6>&1 | Out-Null
        $null
    } catch {
        "$_"
    }
}

$failures = @()
try {
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    $env:REMUDA_CHANNEL = 'stable'
    $env:XDG_DATA_HOME = Join-Path $scratch 'data'
    $env:REMUDA_NO_MODIFY_PATH = '1'
    $env:REMUDA_INSTALL_BUTLER = $null

    $publishedSum = $goodSum
    $matching = Join-Path $scratch 'matching'
    $failed = Install-Into $matching
    if ($failed) { $failures += "a download that matches SHA256SUMS was not installed: $failed" }
    elseif (-not (Test-Path (Join-Path $matching 'remuda.exe'))) { $failures += "a download that matches SHA256SUMS was not installed" }

    $publishedSum = $badSum
    $mismatched = Join-Path $scratch 'mismatched'
    $failed = Install-Into $mismatched
    if ($failed -notlike '*checksum mismatch*') { $failures += "a download that does not match SHA256SUMS was not refused as a mismatch: $failed" }
    if (Test-Path (Join-Path $mismatched 'remuda.exe')) { $failures += "a download that does not match SHA256SUMS was installed" }
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $scratch
}

if ($failures) {
    Write-Host "install.ps1: the checksum step does not do its job in this shell"
    $failures | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "ok - docs/install.ps1 installs a matching download and refuses a mismatched one"
