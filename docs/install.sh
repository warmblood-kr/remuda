#!/bin/sh
# remuda installer — also the upgrader. `remuda upgrade` re-runs this exact
# script, so there is one download-and-verify path rather than two.
#
#   curl -fsSL https://warmblood-kr.github.io/remuda/install.sh | REMUDA_CHANNEL=nightly REMUDA_INSTALL_BUTLER=1 sh
#
#   REMUDA_CHANNEL=stable|nightly   default: the channel already installed, else stable
#   REMUDA_INSTALL_DIR=<dir>        default: ~/.local/bin (Termux: $PREFIX/bin)
#   REMUDA_INSTALL_BUTLER=1         also install warmblood-kr/remuda-butler
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
# Termux runs on Android but normally reports Linux from `uname -s`. Its
# package prefix or `uname -o` identifies the Android environment explicitly.
termux=no
case "${PREFIX:-}" in
*com.termux*) termux=yes ;;
esac
if [ "$termux" != yes ]; then
	os_type=$(uname -o 2>/dev/null || true)
	[ "$os_type" = Android ] && termux=yes
fi
if [ "$termux" = yes ]; then
	os=Android
fi
case "$os/$arch" in
Linux/x86_64) target=x86_64-unknown-linux-gnu ;;
Android/aarch64|Android/arm64) target=aarch64-linux-android ;;
Darwin/arm64) target=aarch64-apple-darwin ;;
*)
	if [ "$channel" = nightly ]; then
		case "$os/$arch" in
		Linux/aarch64|Linux/arm64)
			die "no nightly build for aarch64-linux (ARM Linux) — build from source: cargo install --git https://github.com/$REPO"
			;;
		Darwin/x86_64|Darwin/i386)
			die "no nightly build for x86_64-apple-darwin (Intel macOS) — build from source: cargo install --git https://github.com/$REPO"
			;;
		esac
	fi
	die "no prebuilt binary for $os/$arch — build from source: cargo install --git https://github.com/$REPO"
	;;
esac

version=$(fetch "$INDEX" | tr -d ' \n\r\t' | sed -n "s/.*\"$channel\":\"\([^\"]*\)\".*/\1/p")
# "0.0.0" is a placeholder, not a version: the field being present ("0.0.0"
# passes -n) is not the same question as whether it names a real release.
if [ -z "$version" ] || [ "$version" = 0.0.0 ]; then
	if [ "$channel" = stable ]; then
		die "no stable version published at $INDEX — install nightly instead: curl -fsSL https://warmblood-kr.github.io/remuda/install.sh | REMUDA_CHANNEL=nightly sh"
	fi
	die "no '$channel' version published at $INDEX"
fi

# latest.json is public input. Validate before using the value as a tag or URL
# path component; the release workflow and prune job accept this same shape.
if ! printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-nightly\.[0-9]{14}\.[0-9a-f]{7})?$'; then
	die "invalid '$channel' version in $INDEX: '$version'"
fi

case "$channel" in
stable) tag="v$version" ;;
nightly) tag=$version ;;
esac

base="https://github.com/$REPO/releases/download/$tag"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

if fetch "$base/SHA256SUMS" >"$tmp/SHA256SUMS" 2>/dev/null; then
	asset="remuda-$version-$target.tar.gz"
elif [ "$channel" = nightly ]; then
	# Migration bridge for an index published before nightly releases became
	# immutable version tags. New indexes resolve above; old ones still install
	# from the compatibility alias until the first versioned index is published.
	tag=nightly
	base="https://github.com/$REPO/releases/download/$tag"
	fetch "$base/SHA256SUMS" >"$tmp/SHA256SUMS" || die "cannot download $base/SHA256SUMS"
	asset=$(awk -v t="$target" '{ n = $2; sub(/^\.\//, "", n); if (n ~ ("^remuda-.*-" t "\\.tar\\.gz$")) print n }' "$tmp/SHA256SUMS" | head -n1)
	[ -n "$asset" ] || die "no nightly build published for $target"
	version=${asset#remuda-}
	version=${version%-$target.tar.gz}
else
	die "cannot download $base/SHA256SUMS"
fi

echo "install.sh: fetching remuda $version ($channel, $target)" >&2
fetch "$base/$asset" >"$tmp/$asset" || die "cannot download $base/$asset"

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

install_dir_default="$HOME/.local/bin"
if [ "$termux" = yes ] && [ -n "${PREFIX:-}" ]; then
	install_dir_default="$PREFIX/bin"
fi
install_dir="${REMUDA_INSTALL_DIR:-$install_dir_default}"
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
*)
	echo "install.sh: $install_dir is not on your PATH — add it to your shell profile" >&2
	# The dir is the person's own input and the line is made to be pasted:
	# single quotes keep `$(...)`, backticks and `"` in it inert. A newline
	# cannot be one line at all, so that dir gets no command.
	case "$install_dir" in
	*"
"*) ;;
	*)
		printf "Next: export PATH='%s':\"\$PATH\"\n" "$(printf '%s' "$install_dir" | sed "s/'/'\\\\''/g")" >&2
		echo "Add that line to your shell profile to keep it." >&2
		;;
	esac
	;;
esac

if [ "${REMUDA_INSTALL_BUTLER:-}" = 1 ]; then
	"$install_dir/remuda" mod install warmblood-kr/remuda-butler --force || die "could not install the Butler mod (Next: remuda mod install warmblood-kr/remuda-butler --force)"
	printf '%s\n' 'Next: remuda butler doctor'
fi
