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

# Runs the installer with $onlyPath as the whole PATH. Its output comes back as
# plain lines (no formatter to wrap a long one); a failure as its message.
function Install-With($onlyPath) {
    $env:PATH = $onlyPath
    $result = @{ Failed = $null; Out = '' }
    try {
        $result.Out = (& { Get-Content -Raw $script | Invoke-Expression } 6>&1 | ForEach-Object { "$_" }) -join "`n"
    } catch {
        $result.Failed = "$_"
    }
    $result
}
function Hint-Lines($out) {
    @($out -split "`r?`n" | Where-Object { $_ -like '*git*' -and $_ -like '*remuda mod install warmblood-kr/remuda-butler --force*' })
}

# A PATH with a git.exe on it, for the other side of the gate. It only has to
# be found; nothing in this check lets the installer get as far as running it.
$withGit = Join-Path $scratch 'path-with-git'
New-Item -ItemType Directory -Force -Path $withGit | Out-Null
$gitStub = Join-Path $withGit 'git.exe'
Set-Content -Path $gitStub -Value 'not a real git'
if ($env:OS -ne 'Windows_NT') { chmod +x $gitStub }

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
    $without = Install-With $noGit
    if ($without.Failed) { $failures += "without git: the install failed instead of finishing without the Butler mod: $($without.Failed)" }
    if (-not (Test-Path (Join-Path $installDir 'remuda.exe'))) { $failures += "without git: remuda.exe was not installed" }
    if ((Hint-Lines $without.Out).Count -ne 1) { $failures += "without git: no single line names git and the command to finish with" }
    if ($without.Out -like '*Next: remuda butler doctor*') { $failures += "without git: it pointed at 'remuda butler doctor' though the Butler mod is not installed" }

    # With git, the Butler step must be attempted, not explained away. The
    # stand-in remuda.exe cannot run, so "attempted" shows up as a failure.
    $with = Install-With $withGit
    if (-not $with.Failed) { $failures += "with git: the Butler step was not attempted" }
    if ((Hint-Lines $with.Out).Count -ne 0) { $failures += "with git: it still said git is missing" }

    if ($failures) { $failures += "output without git:`n$($without.Out)`noutput with git:`n$($with.Out)`n$($with.Failed)" }
} finally {
    $env:PATH = $savedPath
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $scratch
}

if ($failures) {
    Write-Host "install.ps1: without git, the Butler step leaves the user stuck"
    $failures | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "ok - docs/install.ps1 without git: Remuda installed, the Butler step explained in one line; with git: attempted"
