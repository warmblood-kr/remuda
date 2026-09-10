# remuda installer for Windows — also the upgrader. `remuda upgrade` re-runs
# this exact script, so there is one download-and-verify path rather than two.
#
#   irm https://warmblood-kr.github.io/remuda/install.ps1 | iex
#
#   $env:REMUDA_CHANNEL     stable|nightly  default: the channel already installed, else stable
#   $env:REMUDA_INSTALL_DIR <dir>           default: ~\.local\bin
#
# This mirrors docs/install.sh line for line, INCLUDING its two bugs-found-by-
# installing: the checksum name is compared as a string (a `./` prefix from
# sha256sum is stripped first, never pattern-matched), and the download is
# landed on disk and checked before anything runs it.

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
function Fetch($url, $outFile) {
    try {
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $outFile
    } catch {
        Die "cannot download $url — $($_.Exception.Message)"
    }
}

$dataDir = if ($env:XDG_DATA_HOME) { $env:XDG_DATA_HOME } else { Join-Path $HOME '.local\share' }
$dataDir = Join-Path $dataDir 'remuda'
$channelFile = Join-Path $dataDir 'channel'

$channel = $env:REMUDA_CHANNEL
if (-not $channel -and (Test-Path $channelFile)) {
    $channel = (Get-Content -Raw $channelFile).Trim()
}
if (-not $channel) { $channel = 'stable' }
if ($channel -ne 'stable' -and $channel -ne 'nightly') {
    Die "unknown channel '$channel' - stable or nightly"
}

# The triples here must match the release workflow's build matrix exactly, or a
# platform this script offers has no asset to download. `scripts/check-install.py`
# fails the build when they drift.
$targets = @{
    'X64' = 'x86_64-pc-windows-msvc'
}
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
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
    if (-not $version) { Die "no '$channel' version published at $Index" }

    $tag = if ($channel -eq 'stable') { "v$version" } else { 'nightly' }
    $base = "https://github.com/$Repo/releases/download/$tag"

    Fetch "$base/SHA256SUMS" (Join-Path $tmp 'SHA256SUMS')

    if ($channel -eq 'nightly') {
        # nightly's tag is fixed and every release replaces its assets, so a
        # version read from latest.json's cached copy (cache-control:
        # max-age=600) can already name a build whose assets no longer exist
        # under that name - for up to ten minutes after every push to main.
        # SHA256SUMS lives on the tag itself and lists exactly what is
        # published right now, so derive the asset name (and the version to
        # report) from that instead of constructing it from the index.
        $asset = $null
        foreach ($line in Get-Content (Join-Path $tmp 'SHA256SUMS')) {
            $fields = $line -split '\s+', 2
            if ($fields.Count -lt 2) { continue }
            $name = $fields[1].Trim() -replace '^\./', ''
            if ($name -match "^remuda-.*-$target\.tar\.gz$") { $asset = $name; break }
        }
        if (-not $asset) { Die "no nightly build published for $target" }
        $version = $asset -replace "^remuda-(.*)-$target\.tar\.gz$", '$1'
    } else {
        $asset = "remuda-$version-$target.tar.gz"
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
    $onPath = ($env:PATH -split ';') -contains $installDir
    if (-not $onPath) {
        Write-Host "install.ps1: $installDir is not on your PATH - add it, e.g."
        Write-Host ('  [Environment]::SetEnvironmentVariable(''PATH'', "$env:PATH;' + $installDir + '", ''User'')')
    }
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
}
