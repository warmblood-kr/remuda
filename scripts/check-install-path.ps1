# Also checks the per-user default paths (%LOCALAPPDATA%) and, when elevated,
# that the data dir is handed to the user.
#
# Runs docs/install.ps1 the way the one-liner does (`| iex`) against a stubbed
# network, and asserts the install dir ends up on PATH: in this session, and on
# Windows persisted for the user, so a NEW PowerShell finds `remuda` too.
#
# The installer ends by printing `Next: remuda butler doctor`. With the install
# dir on no PATH that next step is a CommandNotFoundException, and reopening the
# terminal does not help. Run it yourself, under either shell:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/check-install-path.ps1
#   pwsh -NoProfile -File scripts/check-install-path.ps1

$ErrorActionPreference = 'Stop'

# The User PATH lives in the Windows registry; there is nothing to assert, and
# no installer to run, anywhere else.
if ($env:OS -ne 'Windows_NT') {
    Write-Host "skip - docs/install.ps1 only runs on Windows"
    exit 0
}

$script = Join-Path (Split-Path -Parent $PSScriptRoot) 'docs/install.ps1'
# A Hangul directory name, because that is what a real profile is called
# (C:\Users\<name>) and the path goes through the registry and back. Spelled
# as code points: Windows PowerShell reads this file as ANSI, not UTF-8.
$hangul = -join [char[]](0xC0AC, 0xC6A9, 0xC790)
$scratch = Join-Path ([IO.Path]::GetTempPath()) ("remuda-check-install-path-$hangul-" + [guid]::NewGuid())
$installDir = Join-Path $scratch 'bin'
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

$fakeAsset = Join-Path $scratch 'asset'
Set-Content -Path $fakeAsset -Value 'stands in for the release tarball'
$fakeSum = (Get-FileHash -Algorithm SHA256 $fakeAsset).Hash

# Functions shadow cmdlets and applications, and `iex` runs the installer in
# this scope, so these are what it calls instead of the network and tar.
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

# Each takes out only this check's own entry, from the value as it is now -
# for the user: unexpanded and as the kind it is, then Explorer is told, the
# way the installer does - so a developer's own PATH comes back as it was.
function Remove-FromSessionPath($dir = $installDir) {
    $env:PATH = @(($env:PATH -split ';') | Where-Object { $_ -ne $dir }) -join ';'
}
function Remove-FromUserPath($dir = $installDir) {
    $envKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    if ($envKey.GetValueNames() -contains 'Path') {
        $kind = $envKey.GetValueKind('Path')
        $kept = @(($envKey.GetValue('Path', '', 'DoNotExpandEnvironmentNames') -split ';') | Where-Object { $_ -ne $dir }) -join ';'
        if ($kept) { $envKey.SetValue('Path', $kept, $kind) } else { $envKey.DeleteValue('Path', $false) }
    }
    $envKey.Close()
    $nudge = 'REMUDA_PATH_' + [guid]::NewGuid().ToString('N')
    [Environment]::SetEnvironmentVariable($nudge, '1', 'User')
    [Environment]::SetEnvironmentVariable($nudge, [NullString]::Value, 'User')
}

# In order, each starting from what the one before left. Opted out comes first,
# while the dir is on no PATH yet. Want is how many times the dir must then be
# on the session's PATH and on the user's persisted one.
$cases = @(
    @{ Name = 'REMUDA_NO_MODIFY_PATH=1'; OptOut = '1'; Want = 0 },
    @{ Name = 'fresh install'; Want = 1 },
    @{ Name = 'remuda upgrade'; Want = 1 },
    @{ Name = 'new shell, on the user PATH only'; Before = { Remove-FromSessionPath }; Want = 1 },
    @{ Name = 'on the session PATH only'; Before = { Remove-FromUserPath }; Want = 1 }
)

# With no override at all the installer picks the per-user defaults. Each case
# gets its own profile and, unless Local is $false, its own LOCALAPPDATA; the
# profile is what the installer must fall back to when LOCALAPPDATA is empty.
# Hangul AND a space in the path, as a real %LOCALAPPDATA% can have.
$defaultCases = @(
    @{ Name = 'default paths'; Local = $true },
    @{ Name = 'default paths, data dir already there'; Local = $true; PreCreate = $true },
    @{ Name = 'LOCALAPPDATA empty -> profile\AppData\Local'; Local = $false },
    @{ Name = 'XDG_DATA_HOME still wins for the channel file'; Local = $true; Xdg = $true },
    @{ Name = 'a junction at <data>\remuda keeps its target'; Local = $true; Junction = $true }
)
$me = [Security.Principal.WindowsIdentity]::GetCurrent()
$elevated = ([Security.Principal.WindowsPrincipal]$me).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$was = @{}
foreach ($name in 'LOCALAPPDATA', 'USERPROFILE', 'XDG_DATA_HOME', 'REMUDA_INSTALL_DIR', 'REMUDA_NO_MODIFY_PATH') {
    $was[$name] = [Environment]::GetEnvironmentVariable($name)
}

$failures = @()
try {
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    $env:REMUDA_CHANNEL = 'stable'
    $env:REMUDA_INSTALL_DIR = $installDir
    $env:XDG_DATA_HOME = Join-Path $scratch 'data'
    $env:REMUDA_INSTALL_BUTLER = $null

    foreach ($case in $cases) {
        if ($case.Before) { & $case.Before }
        $env:REMUDA_NO_MODIFY_PATH = $case.OptOut
        Get-Content -Raw $script | Invoke-Expression

        $session = @(($env:PATH -split ';') | Where-Object { $_ -eq $installDir })
        if ($session.Count -ne $case.Want) {
            $failures += "$($case.Name): install dir is on this session's PATH $($session.Count) times, want $($case.Want)"
        }
        $user = @(([Environment]::GetEnvironmentVariable('PATH', 'User') -split ';') | Where-Object { $_ -eq $installDir })
        if ($user.Count -ne $case.Want) {
            $failures += "$($case.Name): install dir is on the user's persisted PATH $($user.Count) times, want $($case.Want)"
        }
    }

    $n = 0
    foreach ($case in $defaultCases) {
        $n++
        $profileDir = Join-Path $scratch "profile-$n"
        $localDir = Join-Path $scratch "app data $hangul $n"
        $base = if ($case.Local) { $localDir } else { Join-Path $profileDir 'AppData\Local' }
        $dataHome = if ($case.Xdg) { Join-Path $scratch "xdg-$n" } else { $base }
        $exeDir = Join-Path $base 'Programs\remuda\bin'
        New-Item -ItemType Directory -Force -Path $profileDir, $base | Out-Null
        if ($case.Junction) {
            # Elevated, the installer must not hand a junction's TARGET to the user.
            $target = Join-Path $scratch "junction-target-$n"
            New-Item -ItemType Directory -Force -Path $target | Out-Null
            New-Item -ItemType Junction -Path (Join-Path $dataHome 'remuda') -Target $target | Out-Null
            $ownerBefore = (Get-Acl -LiteralPath $target).GetOwner([Security.Principal.SecurityIdentifier])
        }
        if ($case.PreCreate) { New-Item -ItemType Directory -Force -Path (Join-Path $dataHome 'remuda') | Out-Null }

        $env:LOCALAPPDATA = if ($case.Local) { $localDir } else { $null }
        $env:USERPROFILE = $profileDir
        $env:XDG_DATA_HOME = if ($case.Xdg) { $dataHome } else { $null }
        $env:REMUDA_INSTALL_DIR = $null
        $env:REMUDA_NO_MODIFY_PATH = $null
        try {
            $out = & { Get-Content -Raw $script | Invoke-Expression } *>&1 | Out-String
            $why = "default paths, $($case.Name)"

            $exe = Join-Path $exeDir 'remuda.exe'
            if (-not (Test-Path -LiteralPath $exe)) { $failures += "${why}: no remuda.exe at $exe" }
            $channelFile = Join-Path (Join-Path $dataHome 'remuda') 'channel'
            if (-not (Test-Path -LiteralPath $channelFile)) {
                $failures += "${why}: no channel file at $channelFile"
            } elseif ((Get-Content -Raw -LiteralPath $channelFile).Trim() -ne 'stable') {
                $failures += "${why}: channel file does not say stable"
            }
            $session = @(($env:PATH -split ';') | Where-Object { $_ -eq $exeDir })
            if ($session.Count -ne 1) { $failures += "${why}: $exeDir is on this session's PATH $($session.Count) times, want 1" }
            $user = @(([Environment]::GetEnvironmentVariable('PATH', 'User') -split ';') | Where-Object { $_ -eq $exeDir })
            if ($user.Count -ne 1) { $failures += "${why}: $exeDir is on the user's persisted PATH $($user.Count) times, want 1" }

            # The cluster code refuses a data dir owned by Administrators, so
            # an elevated install must hand it to the user - found or created.
            $owner = (Get-Acl -LiteralPath (Join-Path $dataHome 'remuda')).GetOwner([Security.Principal.SecurityIdentifier])
            $wantOwner = if ($case.Junction) { $ownerBefore } else { $me.User }
            if ($owner.Value -ne $wantOwner.Value) { $failures += "${why}: $(Join-Path $dataHome 'remuda') is owned by $owner, want $wantOwner" }
            if ($case.Junction -and $elevated -and -not ($out -match 'junction')) { $failures += "${why}: no warning that the data dir is a junction" }
            $warned = $out -match 'elevated shell is not needed'
            if ($elevated -and -not $warned) { $failures += "${why}: elevated, but no warning that an elevated shell is not needed" }
            if (-not $elevated -and $warned) { $failures += "${why}: not elevated, but warned about elevation" }
        } finally {
            Remove-FromSessionPath $exeDir
            Remove-FromUserPath $exeDir
        }
    }
} finally {
    foreach ($name in $was.Keys) { [Environment]::SetEnvironmentVariable($name, $was[$name]) }
    Remove-FromUserPath
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $scratch
}

if ($failures) {
    Write-Host "install.ps1: wrong PATH handling"
    $failures | ForEach-Object { Write-Host "  $_" }
    exit 1
}
Write-Host "ok - docs/install.ps1 puts its install dir on PATH, unless told not to, and installs per-user by default"
