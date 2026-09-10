# Parse-only check for docs/install.ps1 — the PowerShell counterpart of `sh -n`.
#
# The installer is the one file in this repo that nothing else executes on a
# normal build, and the only place a syntax error in it shows up is a stranger's
# terminal. Run it yourself:  pwsh -NoProfile -File scripts/check-powershell.ps1

$script = Join-Path (Split-Path -Parent $PSScriptRoot) 'docs/install.ps1'
if (-not (Test-Path $script)) {
    Write-Host "install.ps1: missing at $script"
    exit 1
}

$parseErrors = $null
[void][System.Management.Automation.Language.Parser]::ParseFile(
    (Resolve-Path $script), [ref]$null, [ref]$parseErrors)

if ($parseErrors) {
    Write-Host "install.ps1: syntax error"
    $parseErrors | ForEach-Object { Write-Host "  $_" }
    exit 1
}

Write-Host "ok - docs/install.ps1 parses"
