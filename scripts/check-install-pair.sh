#!/bin/sh
# install.sh lands remuda and remuda-session-host as one pair and keeps the
# pair it replaced as *.prev for rollback. An asset from before the pair (no
# host) must not leave a host from another build beside the new daemon.
# No network: a curl shim serves a fake release.
#
# Run it yourself:  scripts/check-install-pair.sh
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$@"; }
else
	sha256() { shasum -a 256 "$@"; }
fi

version=0.1.0-nightly.20260101000000.abcdef1
mkdir -p "$tmp/shim" "$tmp/home" "$tmp/data"
cat >"$tmp/shim/curl" <<'SHIM'
#!/bin/sh
for a in "$@"; do url=$a; done
case "$url" in
*/latest.json) printf '{"stable":"0.0.0","nightly":"%s"}\n' "$FAKE_VERSION" ;;
*/SHA256SUMS) cat "$FAKE_DIR/SHA256SUMS" ;;
*) cat "$FAKE_DIR/asset" ;;
esac
SHIM
chmod +x "$tmp/shim/curl"
export FAKE_VERSION="$version" FAKE_DIR="$tmp"
export HOME="$tmp/home" XDG_DATA_HOME="$tmp/data" REMUDA_CHANNEL=nightly
export REMUDA_INSTALL_DIR="$tmp/bin"
unset REMUDA_INSTALL_BUTLER

failed=0
fail() {
	echo "check-install-pair: $*" >&2
	failed=1
}

# Publish a release whose binaries say BUILD; with "nohost", only remuda.
publish() {
	rm -rf "$tmp/pkg" && mkdir -p "$tmp/pkg"
	printf '#!/bin/sh\necho remuda %s\n' "$1" >"$tmp/pkg/remuda"
	files=remuda
	if [ "${2:-}" != nohost ]; then
		printf '#!/bin/sh\necho host %s\n' "$1" >"$tmp/pkg/remuda-session-host"
		files="remuda remuda-session-host"
	fi
	# shellcheck disable=SC2086
	tar -czf "$tmp/asset" -C "$tmp/pkg" $files
	sum=$(sha256 "$tmp/asset" | awk '{ print $1 }')
	for target in x86_64-unknown-linux-gnu aarch64-linux-android aarch64-apple-darwin; do
		echo "$sum  remuda-$version-$target.tar.gz"
	done >"$tmp/SHA256SUMS"
}

install() {
	out=$(PATH="$tmp/shim:$PATH" sh "$ROOT/docs/install.sh" 2>&1) || fail "$1: the install failed: $out"
}

# Assert FILE in the install dir prints WANT, or is absent when WANT is "-".
has() {
	case_name=$1 file="$tmp/bin/$2" want=$3
	if [ "$want" = - ]; then
		[ ! -e "$file" ] || fail "$case_name: $2 should be absent, says '$(sh "$file")'"
	elif [ ! -x "$file" ]; then
		fail "$case_name: $2 is missing or not executable"
	else
		got=$(sh "$file")
		[ "$got" = "$want" ] || fail "$case_name: $2 says '$got', want '$want'"
	fi
}

publish 1
install "fresh install"
has "fresh install" remuda "remuda 1"
has "fresh install" remuda-session-host "host 1"
has "fresh install" remuda.prev -
has "fresh install" remuda-session-host.prev -

publish 2
install "upgrade"
has "upgrade" remuda "remuda 2"
has "upgrade" remuda-session-host "host 2"
has "upgrade" remuda.prev "remuda 1"
has "upgrade" remuda-session-host.prev "host 1"

publish 3 nohost
install "release without a host"
has "release without a host" remuda "remuda 3"
has "release without a host" remuda-session-host -
has "release without a host" remuda.prev "remuda 2"
has "release without a host" remuda-session-host.prev "host 2"

publish 4
install "pair after a hostless release"
has "pair after a hostless release" remuda "remuda 4"
has "pair after a hostless release" remuda-session-host "host 4"
has "pair after a hostless release" remuda.prev "remuda 3"
has "pair after a hostless release" remuda-session-host.prev -

leftover=$(find "$tmp/bin" -name '.*' -type f)
[ -z "$leftover" ] || fail "staging files left behind: $leftover"

[ "$failed" -eq 0 ] || exit 1
echo "ok — install.sh lands the remuda/session-host pair and keeps the previous pair as .prev"
