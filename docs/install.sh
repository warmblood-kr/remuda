#!/bin/sh
# remuda installer — also the upgrader. `remuda upgrade` re-runs this exact
# script, so there is one download-and-verify path rather than two.
#
#   curl -fsSL https://warmblood-kr.github.io/remuda/install.sh | sh
#
#   REMUDA_CHANNEL=stable|nightly   default: the channel already installed, else stable
#   REMUDA_INSTALL_DIR=<dir>        default: ~/.local/bin
#
# Windows has its own installer, docs/install.ps1, because a `uname` case arm
# cannot run there. The two hold disjoint halves of one platform list and
# `scripts/check-install.py` fails the build if either half drifts from the
# release matrix. See steps/010.

set -eu

REPO=warmblood-kr/remuda
INDEX=https://warmblood-kr.github.io/remuda/latest.json

die() {
	echo "install.sh: $*" >&2
	exit 1
}

if command -v curl >/dev/null 2>&1; then
	fetch() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
	fetch() { wget -qO- "$1"; }
else
	die "need curl or wget"
fi

# `sha256sum` on Linux, `shasum` on macOS. No fallback: a checksum step that
# quietly skips verification is worse than one that stops.
if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$@"; }
elif command -v shasum >/dev/null 2>&1; then
	sha256() { shasum -a 256 "$@"; }
else
	die "need sha256sum or shasum to verify the download"
fi

data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/remuda"
channel_file="$data_dir/channel"

channel="${REMUDA_CHANNEL:-}"
if [ -z "$channel" ] && [ -r "$channel_file" ]; then
	channel=$(cat "$channel_file")
fi
channel="${channel:-stable}"
case "$channel" in
stable | nightly) ;;
*) die "unknown channel '$channel' — stable or nightly" ;;
esac

# The triples here must match the release workflow's build matrix exactly, or a
# platform this script offers has no asset to download. `scripts/check-install.py`
# fails the build when they drift.
os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
Darwin/arm64) target=aarch64-apple-darwin ;;
*) die "no prebuilt binary for $os/$arch — build from source: cargo install --git https://github.com/$REPO" ;;
esac

version=$(fetch "$INDEX" | tr -d ' \n\r\t' | sed -n "s/.*\"$channel\":\"\([^\"]*\)\".*/\1/p")
[ -n "$version" ] || die "no '$channel' version published at $INDEX"

case "$channel" in
stable) tag="v$version" ;;
nightly) tag=nightly ;;
esac

asset="remuda-$version-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$tag"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "install.sh: fetching remuda $version ($channel, $target)" >&2
fetch "$base/$asset" >"$tmp/$asset" || die "cannot download $base/$asset"
fetch "$base/SHA256SUMS" >"$tmp/SHA256SUMS" || die "cannot download $base/SHA256SUMS"

cd "$tmp" || die "cannot enter $tmp"
# Exact string equality on the filename, not a regex match: the asset name is
# full of dots, and `sha256sum ./*` writes a `./` prefix that a naive pattern
# misses entirely — which is how this line was found, by installing for real.
awk -v want="$asset" '{ sub(/^\.\//, "", $2); if ($2 == want) print $1 "  " $2 }' \
	SHA256SUMS >expected
[ -s expected ] || die "$asset is not listed in SHA256SUMS"
sha256 -c expected >/dev/null || die "checksum mismatch on $asset — refusing to install"
tar -xzf "$asset"
[ -f remuda ] || die "$asset does not contain ./remuda"

install_dir="${REMUDA_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$install_dir" "$data_dir"

# Land it by rename, never by writing in place: `remuda upgrade` runs this while
# that very binary is executing, and truncating it would be ETXTBSY (or worse,
# a half-written binary). A rename swaps the directory entry and leaves the
# running process on its old inode.
cp remuda "$install_dir/.remuda.incoming"
chmod 755 "$install_dir/.remuda.incoming"
mv -f "$install_dir/.remuda.incoming" "$install_dir/remuda"

echo "$channel" >"$channel_file"

echo "install.sh: remuda $version -> $install_dir/remuda ($channel channel)" >&2
case ":$PATH:" in
*":$install_dir:"*) ;;
*) echo "install.sh: $install_dir is not on your PATH — add it to your shell profile" >&2 ;;
esac
