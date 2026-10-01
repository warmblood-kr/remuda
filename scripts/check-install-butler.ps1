# Runs docs/install.ps1 with REMUDA_INSTALL_BUTLER=1 on a machine without git
# - which is a stock Windows - and asserts what the user is left with.
#
# `remuda mod install` clones with git. Without it the Butler step cannot
# work, but Remuda itself is installed by then: the installer must say what is
# missing and how to finish, in one line, and not fail the install over it.
# Stubbed network, no registry (REMUDA_NO_MODIFY_PATH), so any platform:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check-install-butler.ps1
#   pwsh -NoProfile -File scripts/check-install-butler.ps1

$ErrorActionPreference = 'Stop'

$script = Join-Path (Split-Path -Parent $PSScriptRoot) 'docs/install.ps1'
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("remuda-check-install-butler-" + [guid]::NewGuid())
$installDir = Join-Path $scratch 'bin'
$noGit = Join-Path $scratch 'empty-path'
New-Item -ItemType Directory -Force -Path $noGit | Out-Null

$fakeAsset = Join-Path $scratch 'asset'
Set-Content -Path $fakeAsset -Value 'stands in for the release tarball'
$fakeSum = (Get-FileHash -Algorithm SHA256 $fakeAsset).Hash

# Functions shadow cmdlets and applications, and `iex` runs the installer in
# a scope that sees them, so these are what it calls instead of the network
# and tar. The remuda.exe they leave is not a program: running it is an error.
function Invoke-WebRequest {
    param([switch]$UseBasicParsing, $Uri, $OutFile)
    if ($Uri -like '*/latest.json') {
        Set-Content -Path $OutFile -Value '{"stable":"9.9.9","nightly":"9.9.9"}'
    } elseif ($Uri -like '*/SHA256SUMS') {
        Set-Content -Path $OutFile -Value "$fakeSum  ./remuda-9.9.9-x86_64-pc-windows-msvc.tar.gz"
    } else {
        Copy-Item $fakeAsset $OutFile
    }
}
function tar {
    # Called as: tar -xzf <asset> -C <dir>
    Set-Content -Path (Join-Path $args[3] 'remuda.exe') -Value 'not a real binary'
    $global:LASTEXITCODE = 0
}

$savedPath = $env:PATH
$failures = @()
try {
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    $env:REMUDA_CHANNEL = 'stable'
    $env:REMUDA_INSTALL_DIR = $installDir
    $env:XDG_DATA_HOME = Join-Path $scratch 'data'
    $env:REMUDA_NO_MODIFY_PATH = '1'
    $env:REMUDA_INSTALL_BUTLER = '1'
    # No git anywhere on PATH: an empty directory is all of it.
    $env:PATH = $noGit

    $failed = $null
    $out = try {
        & { Get-Content -Raw $script | Invoke-Expression } 6>&1 | Out-String
    } catch {
        $failed = "$_"
    }

    if ($failed) { $failures += "the install failed instead of finishing without the Butler mod: $failed" }
    if (-not (Test-Path (Join-Path $installDir 'remuda.exe'))) { $failures += "remuda.exe was not installed" }
    $hint = @($out -split "`r?`n" | Where-Object { $_ -like '*git*' -and $_ -like '*remuda mod install warmblood-kr/remuda-butler --force*' })
    if ($hint.Count -ne 1) { $failures += "no single line names git and the command to finish with" }
    if ($out -like '*Next: remuda butler doctor*') { $failures += "it pointed at 'remuda butler doctor' though the Butler mod is not installed" }
    if ($failures) { $failures += "installer output:`n$out" }
} finally {
    $env:PATH = $savedPath
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $scratch
}

if ($failures) {
    Write-Host "install.ps1: without git, the Butler step leaves the user stuck"
    $failures | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "ok - docs/install.ps1 without git: Remuda installed, the Butler step explained in one line"
