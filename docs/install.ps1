# remuda installer for Windows — also the upgrader. `remuda upgrade` re-runs
# this exact script, so there is one download-and-verify path rather than two.
#
#   $env:REMUDA_CHANNEL='nightly'; $env:REMUDA_INSTALL_BUTLER='1'; irm https://warmblood-kr.github.io/remuda/install.ps1 | iex
#
#   $env:REMUDA_CHANNEL     stable|nightly  default: the channel already installed, else stable
#   $env:REMUDA_INSTALL_DIR <dir>           default: ~\.local\bin
#   $env:REMUDA_INSTALL_BUTLER=1            also install warmblood-kr/remuda-butler
#   $env:REMUDA_NO_MODIFY_PATH=1            leave PATH alone; print how to add the install dir
#
# This mirrors docs/install.sh: resolve the channel version first, then verify
# its checksum before installing the binary.

$ErrorActionPreference = 'Stop'

$Repo  = 'warmblood-kr/remuda'
$Index = 'https://warmblood-kr.github.io/remuda/latest.json'

function Die($message) {
    Write-Host "install.ps1: $message" -ForegroundColor Red
    exit 1
}

# `-UseBasicParsing` for Windows PowerShell 5.1, which is what a fresh machine
# has. With $ErrorActionPreference = 'Stop' a 404 raises rather than writing an
# error page to the output file — the shell script's `curl | sh` bug, avoided.
function TryFetch($url, $outFile) {
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $outFile
        return $true
    } catch {
        return $false
    }
}

function Fetch($url, $outFile) {
    if (-not (TryFetch $url $outFile)) {
        Die "cannot download $url"
    }
}

$dataDir = if ($env:XDG_DATA_HOME) { $env:XDG_DATA_HOME } else { Join-Path $HOME '.local\share' }
$dataDir = Join-Path $dataDir 'remuda'
$channelFile = Join-Path $dataDir 'channel'

$channel = $env:REMUDA_CHANNEL
if (-not $channel -and (Test-Path $channelFile)) {
    # Test-Path only proves the file exists, not that it has content — an
    # empty channel file makes Get-Content -Raw return $null, and $null.Trim()
    # is the same "cannot call a method on a null-valued expression" crash as
    # the arch bug below.
    $raw = Get-Content -Raw $channelFile
    if ($raw) { $channel = $raw.Trim() }
}
if (-not $channel) { $channel = 'stable' }
if ($channel -ne 'stable' -and $channel -ne 'nightly') {
    Die "unknown channel '$channel' - stable or nightly"
}

# The triples here must match the release workflow's build matrix exactly, or a
# platform this script offers has no asset to download. `scripts/check-install.py`
# fails the build when they drift.
$targets = @{
    'AMD64' = 'x86_64-pc-windows-msvc'
}
# Not [RuntimeInformation]::OSArchitecture: in an interactive session,
# PowerShell can bind that type name to PSReadLine's own same-named shadow
# class instead of the real .NET type — a silent $null member read, then this
# exact "cannot call a method on a null-valued expression" from .ToString().
# PROCESSOR_ARCHITEW6432 carries the true OS architecture when the process is
# 32-bit under WOW64; PROCESSOR_ARCHITECTURE otherwise.
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
$target = $targets[$arch]
if (-not $target) {
    Die "no prebuilt binary for Windows/$arch - build from source: cargo install --git https://github.com/$Repo"
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("remuda-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    $indexFile = Join-Path $tmp 'latest.json'
    Fetch $Index $indexFile
    $version = (Get-Content -Raw $indexFile | ConvertFrom-Json).$channel
    # "0.0.0" is a placeholder, not a version: the field being present ('0.0.0'
    # is truthy) is not the same question as whether it names a real release.
    if (-not $version -or $version -eq '0.0.0') {
        if ($channel -eq 'stable') {
            Die "no stable version published at $Index - install nightly instead: `$env:REMUDA_CHANNEL='nightly'; irm https://warmblood-kr.github.io/remuda/install.ps1 | iex"
        }
        Die "no '$channel' version published at $Index"
    }

    $tag = if ($channel -eq 'stable') { "v$version" } else { $version }
    $base = "https://github.com/$Repo/releases/download/$tag"

    $manifestPath = Join-Path $tmp 'SHA256SUMS'
    if (TryFetch "$base/SHA256SUMS" $manifestPath) {
        $asset = "remuda-$version-$target.tar.gz"
    } elseif ($channel -eq 'nightly') {
        # Migration bridge for indexes published before nightly releases became
        # immutable version tags. New indexes resolve above; old ones keep
        # working through the rolling compatibility alias.
        $tag = 'nightly'
        $base = "https://github.com/$Repo/releases/download/$tag"
        Fetch "$base/SHA256SUMS" $manifestPath
        $asset = $null
        foreach ($line in Get-Content $manifestPath) {
            $fields = $line -split '\s+', 2
            if ($fields.Count -lt 2) { continue }
            $name = $fields[1].Trim() -replace '^\./', ''
            if ($name -match "^remuda-.*-$target\.tar\.gz$") { $asset = $name; break }
        }
        if (-not $asset) { Die "no nightly build published for $target" }
        $version = $asset -replace "^remuda-(.*)-$target\.tar\.gz$", '$1'
    } else {
        Die "cannot download $base/SHA256SUMS"
    }

    Write-Host "install.ps1: fetching remuda $version ($channel, $target)"
    Fetch "$base/$asset" (Join-Path $tmp $asset)

    # Exact string equality on the filename, not a regex match: the asset name
    # is full of dots, and `sha256sum ./*` writes a `./` prefix that a naive
    # pattern misses entirely - which is how this line was found, by installing
    # for real.
    $expected = $null
    foreach ($line in Get-Content (Join-Path $tmp 'SHA256SUMS')) {
        $fields = $line -split '\s+', 2
        if ($fields.Count -lt 2) { continue }
        $name = $fields[1].Trim() -replace '^\./', ''
        if ($name -eq $asset) { $expected = $fields[0].Trim() }
    }
    if (-not $expected) { Die "$asset is not listed in SHA256SUMS" }

    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash
    if ($actual -ine $expected) {
        Die "checksum mismatch on $asset - refusing to install"
    }

    # `tar` has shipped in Windows since 1803, so there is no extra dependency
    # here and one asset shape serves every platform.
    try {
        tar -xzf (Join-Path $tmp $asset) -C $tmp
    } catch {
        Die "cannot unpack $asset - $($_.Exception.Message)"
    }
    if ($LASTEXITCODE -ne 0) { Die "cannot unpack $asset" }
    $unpacked = Join-Path $tmp 'remuda.exe'
    if (-not (Test-Path $unpacked)) { Die "$asset does not contain remuda.exe" }

    $installDir = if ($env:REMUDA_INSTALL_DIR) { $env:REMUDA_INSTALL_DIR } else { Join-Path $HOME '.local\bin' }
    New-Item -ItemType Directory -Force -Path $installDir, $dataDir | Out-Null
    # Absolute, because it is about to be written to PATH, where a relative
    # entry means a different directory in every shell.
    $installDir = (Get-Item -Force -LiteralPath $installDir).FullName
    $installed = Join-Path $installDir 'remuda.exe'

    # Windows refuses to overwrite a RUNNING executable but allows renaming it,
    # which is this platform's version of install.sh's land-by-rename: `remuda
    # upgrade` runs this while that very binary is executing.
    $retired = Join-Path $installDir ('.remuda.exe.old-' + [guid]::NewGuid())
    if (Test-Path $installed) { Move-Item -Force $installed $retired }
    try {
        Move-Item -Force $unpacked $installed
    } catch {
        if (Test-Path $retired) { Move-Item -Force $retired $installed }
        Die "cannot place remuda.exe in $installDir - $($_.Exception.Message)"
    }
    Get-ChildItem -Path $installDir -Filter '.remuda.exe.old-*' -Force -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue

    Set-Content -Path $channelFile -Value $channel -NoNewline

    Write-Host "install.ps1: remuda $version -> $installed ($channel channel)"
    # Nothing on a stock Windows has ~\.local\bin on PATH, and a hint to add it
    # is one more step between the one-liner and `remuda` being a command. So
    # persist it for the user, and add it to this session for the next step.
    # Already on this session's PATH means there is nothing to do, however it
    # got there - which is also what keeps `remuda upgrade` from undoing an
    # opt-out. REMUDA_NO_MODIFY_PATH is that opt-out.
    if (($env:PATH -split ';') -notcontains $installDir) {
        if ($env:REMUDA_NO_MODIFY_PATH) {
            Write-Host "install.ps1: $installDir is not on your PATH - add it, e.g."
            Write-Host "  [Environment]::SetEnvironmentVariable('PATH', [Environment]::GetEnvironmentVariable('PATH', 'User') + ';$installDir', 'User')"
        } else {
            # Read from the registry unexpanded and written back as the kind it
            # was, so the user's own entries survive as-is; only the User PATH
            # is touched.
            $envKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
            $userPath = $envKey.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
            $pathKind = if ($envKey.GetValueNames() -contains 'Path') { $envKey.GetValueKind('Path') } else { 'ExpandString' }
            if (($userPath -split ';') -notcontains $installDir) {
                $newUserPath = if ($userPath) { $userPath.TrimEnd(';') + ';' + $installDir } else { $installDir }
                $envKey.SetValue('Path', $newUserPath, $pathKind)
                # A registry write alone reaches nobody. Explorer re-reads the
                # environment on WM_SETTINGCHANGE, which .NET broadcasts when it
                # sets a User variable - so set and clear a throwaway one.
                $nudge = 'REMUDA_PATH_' + [guid]::NewGuid().ToString('N')
                [Environment]::SetEnvironmentVariable($nudge, '1', 'User')
                [Environment]::SetEnvironmentVariable($nudge, [NullString]::Value, 'User')
                Write-Host "install.ps1: added $installDir to your user PATH - a terminal app that is already open may need a restart to see it"
            }
            $envKey.Close()
            $env:PATH = $env:PATH.TrimEnd(';') + ';' + $installDir
        }
    }

    if ($env:REMUDA_INSTALL_BUTLER -eq '1') {
        & $installed mod install warmblood-kr/remuda-butler --force
        if ($LASTEXITCODE -ne 0) { Die 'could not install the Butler mod (Next: remuda mod install warmblood-kr/remuda-butler --force)' }
        Write-Output 'Next: remuda butler doctor'
    }
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
}
