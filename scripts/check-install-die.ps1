# Makes docs/install.ps1 fail (an unknown channel: no network, any platform)
# and asserts what its failure does to the shell that ran it.
#
# The one-liner is `irm ... | iex`: the script runs IN the user's session. If
# it fails by calling `exit`, their PowerShell window closes and the message
# goes with it. `remuda upgrade` runs it as a file instead, and reads the exit
# code. Run it yourself, under either shell:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check-install-die.ps1
#   pwsh -NoProfile -File scripts/check-install-die.ps1

# Not 'Stop': Windows PowerShell turns a child's stderr into error records.
$ErrorActionPreference = 'Continue'

$script = Join-Path (Split-Path -Parent $PSScriptRoot) 'docs/install.ps1'
$shell = (Get-Process -Id $PID).Path
$env:REMUDA_CHANNEL = 'no-such-channel'
$env:REMUDA_CHECK_INSTALLER = $script
$message = "install.ps1: unknown channel 'no-such-channel'"
$failures = @()

# As the one-liner: the session must outlive the failure, and be told why.
$piped = 'try { Get-Content -Raw $env:REMUDA_CHECK_INSTALLER | Invoke-Expression } catch { Write-Output $_ }; Write-Output SESSION-SURVIVED'
$out = (& $shell -NoProfile -ExecutionPolicy Bypass -Command $piped 2>&1 | Out-String)
if ($out -notlike '*SESSION-SURVIVED*') { $failures += "piped to iex: the failure ended the session:`n$out" }
if ($out -notlike "*$message*") { $failures += "piped to iex: the failure did not say why:`n$out" }

# As a file: nonzero, and the same reason.
$out = (& $shell -NoProfile -ExecutionPolicy Bypass -File $script 2>&1 | Out-String)
if ($LASTEXITCODE -eq 0) { $failures += "run as a file: a failed install exited 0" }
if ($out -notlike "*$message*") { $failures += "run as a file: the failure did not say why:`n$out" }

if ($failures) {
    Write-Host "install.ps1: a failure does the wrong thing to its shell"
    $failures | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "ok - docs/install.ps1 fails without ending the session, and exits nonzero as a file"
