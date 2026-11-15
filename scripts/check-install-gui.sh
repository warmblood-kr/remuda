#!/bin/sh
# Offline macOS archive contract, collision and interruption regression.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$tmp/shim" "$tmp/data" "$tmp/bin" "$tmp/pkg" "$tmp/installer-tmp"
export TMPDIR="$tmp/installer-tmp"
export XDG_DATA_HOME="$tmp/data" XDG_CONFIG_HOME="$tmp/config" XDG_STATE_HOME="$tmp/state" XDG_CACHE_HOME="$tmp/cache"
export REMUDA_RUNTIME_DIR="$tmp/runtime" REMUDA_INSTALL_DIR="$tmp/bin" REMUDA_CHANNEL=nightly
export FAKE_DIR="$tmp" FAKE_VERSION=0.1.0-nightly.20260101000000.abcdef1 FAKE_INSTALLER="$root/docs/install.sh"
unset REMUDA_INSTALL_BUTLER
cat >"$tmp/shim/uname" <<'SH'
#!/bin/sh
case "$1" in -s) echo Darwin ;; -m) echo arm64 ;; -o) exit 1 ;; *) exit 1 ;; esac
SH
cat >"$tmp/shim/curl" <<'SH'
#!/bin/sh
out=
while [ "$#" -gt 0 ]; do
  case "$1" in -o) out=$2; shift 2 ;; *) url=$1; shift ;; esac
done
case "$url" in
  */install.sh) [ -z "$out" ] && cat "$FAKE_INSTALLER" || cp "$FAKE_INSTALLER" "$out" ;;
  */latest.json) printf '{"stable":"0.0.0","nightly":"%s"}\n' "$FAKE_VERSION" ;;
  */SHA256SUMS) cat "$FAKE_DIR/SHA256SUMS" ;;
  *) cat "$FAKE_DIR/asset" ;;
esac
SH
cat >"$tmp/shim/ln" <<'SH'
#!/bin/sh
case "$2" in
  */.remuda-gui/builds/*/provenance)
    case "${FAKE_LINK_MODE:-}" in
      crash-manifest-before) kill -KILL "$PPID"; exit 1 ;;
      crash-manifest-after) /bin/ln "$@"; kill -KILL "$PPID"; exit 1 ;;
    esac ;;
  */.remuda-gui/builds/*/remuda-gui)
    case "${FAKE_LINK_MODE:-}" in
      fail) exit 1 ;;
      crash-before) kill -KILL "$PPID"; exit 1 ;;
      crash-after) /bin/ln "$@"; kill -KILL "$PPID"; exit 1 ;;
    esac ;;
esac
exec /bin/ln "$@"
SH
chmod +x "$tmp/shim/"*
export PATH="$tmp/shim:$PATH"
if command -v sha256sum >/dev/null 2>&1; then sha256() { sha256sum "$@"; }
else sha256() { shasum -a 256 "$@"; }; fi

pack() {
	# shellcheck disable=SC2086
	tar -czf "$tmp/asset" -C "$tmp/pkg" $files
	sum=$(sha256 "$tmp/asset" | awk '{print $1}')
	printf '%s  remuda-%s-aarch64-apple-darwin.tar.gz\n' "$sum" "$FAKE_VERSION" >"$tmp/SHA256SUMS"
}
# Build a checksummed release. The fake supervisor checks the requested
# protocol and logs each invocation; the real bounded supervisor has its own
# macOS process-lifecycle tests.
publish() {
	id="$FAKE_VERSION+$1"
	rm -rf "$tmp/pkg"
	mkdir "$tmp/pkg"
	printf '#!/bin/sh\nprintf "remuda %s\\n"\n' "$id" >"$tmp/pkg/remuda"
	printf '#!/bin/sh\n[ "$1" = version ] && { printf "%s\\n"; exit 0; }\nprintf "host %s\\n"\n' "$id" "$id" >"$tmp/pkg/remuda-session-host"
	files="remuda remuda-session-host"
	if [ "${2:-gui}" != old ]; then
		printf '#!/bin/sh\nprintf "%s\\n"\n# %s\n' "$id" "${2:-gui}" >"$tmp/pkg/remuda-gui"
		printf '%s\n' remuda-gui-archive-v1 >"$tmp/pkg/remuda-gui.contract"
		printf '%s\n' "$id" >"$tmp/pkg/remuda-gui.build"
		cat >"$tmp/pkg/remuda-installer-probe" <<'SH'
#!/bin/sh
printf '%s\n' "$1" >>"$FAKE_DIR/probes"
[ "$2" = --verified-archive-v1 ] || exit 1
[ "${FAKE_PROBE_FAIL:-}" != "$1" ] || exit 1
case "$1" in
  --verify-cli) arg=--version; want="remuda $4" ;;
  --verify-host) arg=version; want=$4 ;;
  --verify) arg=--internal-build-id; want=$4 ;;
  *) exit 1 ;;
esac
[ "$("$3" "$arg")" = "$want" ]
SH
		files="$files remuda-gui remuda-gui.contract remuda-gui.build remuda-installer-probe"
	fi
	chmod +x "$tmp/pkg/remuda" "$tmp/pkg/remuda-session-host"
	[ ! -f "$tmp/pkg/remuda-gui" ] || chmod +x "$tmp/pkg/remuda-gui" "$tmp/pkg/remuda-installer-probe"
	pack
}
install_ok() { sh "$root/docs/install.sh" >"$tmp/out" 2>&1 || { cat "$tmp/out"; exit 1; }; }
install_fails() { if sh "$root/docs/install.sh" >"$tmp/out" 2>&1; then cat "$tmp/out"; exit 1; fi; }
slot() { printf '%s/.remuda-gui/builds/build-%s+%s/remuda-gui' "$REMUDA_INSTALL_DIR" "$FAKE_VERSION" "$1"; }
same() { cmp -s "$1" "$2" || { echo "different: $1 $2" >&2; exit 1; }; }
# A manual sibling and an installed mod are outside the managed layout.
printf 'manual GUI\n' >"$tmp/bin/remuda-gui"
mkdir -p "$tmp/data/remuda/mods/gui"
printf 'existing mod\n' >"$tmp/data/remuda/mods/gui/sentinel"
publish a
install_ok
same "$tmp/pkg/remuda-gui" "$(slot a)"
[ "$(cat "$tmp/bin/remuda-gui")" = 'manual GUI' ]
[ "$(cat "$tmp/data/remuda/mods/gui/sentinel")" = 'existing mod' ]
[ "$(wc -l < "$tmp/probes")" -eq 3 ]
# Reinstalling exactly the same bytes must not replace the managed inode.
if stat -f '%i %Lp %m' "$(slot a)" >/dev/null 2>&1; then stamp() { stat -f '%i %Lp %m' "$1"; }
else stamp() { stat -c '%i %a %Y' "$1"; }; fi
before=$(stamp "$(slot a)")
cp "$(dirname "$(slot a)")/provenance" "$tmp/provenance"
install_ok
[ "$(stamp "$(slot a)")" = "$before" ]
same "$(dirname "$(slot a)")/provenance" "$tmp/provenance"
# Same key with different bytes refuses before any current or .prev changes.
cp "$tmp/bin/remuda" "$tmp/current"
cp "$tmp/bin/remuda.prev" "$tmp/previous"
publish a different
install_fails
same "$tmp/bin/remuda" "$tmp/current"
same "$tmp/bin/remuda.prev" "$tmp/previous"
[ "$(stamp "$(slot a)")" = "$before" ]
# The same key and GUI cannot silently change the CLI or host bytes either.
for file in remuda remuda-session-host; do
	publish a
	printf '# changed %s\n' "$file" >>"$tmp/pkg/$file"
	pack
	install_fails
	same "$tmp/bin/remuda" "$tmp/current"
	same "$tmp/bin/remuda.prev" "$tmp/previous"
	same "$(dirname "$(slot a)")/provenance" "$tmp/provenance"
done
# A retained host whose name asserts this build must match before any swap.
publish h
kept="$tmp/bin/remuda-session-host-$FAKE_VERSION+h"
printf 'unrelated retained host\n' >"$kept"
install_fails
[ "$(cat "$kept")" = 'unrelated retained host' ]
same "$tmp/bin/remuda" "$tmp/current"
# An identical installed component establishes that its paired component must
# also match, without executing the old CLI or host.
for changed in remuda remuda-session-host; do
	publish u
	install_root="$tmp/unmanaged-$changed"
	mkdir "$install_root"
	cp "$tmp/pkg/remuda" "$install_root/remuda"
	cp "$tmp/pkg/remuda-session-host" "$install_root/remuda-session-host"
	printf '# local change\n' >>"$install_root/$changed"
	cp "$install_root/$changed" "$tmp/unmanaged-before"
	REMUDA_INSTALL_DIR="$install_root"
	export REMUDA_INSTALL_DIR
	install_fails
	REMUDA_INSTALL_DIR="$tmp/bin"
	export REMUDA_INSTALL_DIR
	same "$install_root/$changed" "$tmp/unmanaged-before"
	[ ! -e "$install_root/.remuda-gui" ]
done
# Unknown old pair: publish only core, report deferral, and preserve manual GUI.
publish u
install_root="$tmp/unknown path \$(literal)"
mkdir "$install_root"
printf '#!/bin/sh\nprintf old-cli-executed >"$FAKE_DIR/old-cli-called"\n' >"$install_root/remuda"
printf '#!/bin/sh\nprintf old-host-executed >"$FAKE_DIR/old-host-called"\nprintf "old-build\\n"\n' >"$install_root/remuda-session-host"
chmod +x "$install_root/remuda" "$install_root/remuda-session-host"
printf 'manual GUI\n' >"$install_root/remuda-gui"
REMUDA_INSTALL_DIR="$install_root"
export REMUDA_INSTALL_DIR
install_ok
grep -q 'GUI deferred.*provenance is unknown' "$tmp/out"
grep -q 'build-key identity remains unverified' "$tmp/out"
[ ! -e "$(slot u)" ]
[ "$(cat "$install_root/remuda-gui")" = 'manual GUI' ]
[ ! -e "$tmp/old-cli-called" ]
[ -e "$tmp/old-host-called" ] # Existing core-only keep_versioned behavior.
same "$install_root/remuda" "$tmp/pkg/remuda"
install_ok
same "$(slot u)" "$tmp/pkg/remuda-gui"
[ "$(cat "$install_root/remuda-gui")" = 'manual GUI' ]
REMUDA_INSTALL_DIR="$tmp/bin"
export REMUDA_INSTALL_DIR
# Corrupt prior provenance cannot establish a managed old pair.
publish b
install_root="$tmp/corrupt-prior"
mkdir -p "$install_root/.remuda-gui/builds"
cp "$tmp/bin/remuda" "$install_root/remuda"
cp "$tmp/bin/remuda-session-host" "$install_root/remuda-session-host"
cp -R "$tmp/bin/.remuda-gui/builds/build-$FAKE_VERSION+a" "$install_root/.remuda-gui/builds/"
printf 'corrupt\n' >"$install_root/.remuda-gui/builds/build-$FAKE_VERSION+a/provenance"
REMUDA_INSTALL_DIR="$install_root"
export REMUDA_INSTALL_DIR
install_ok
grep -q 'GUI deferred.*provenance is unknown' "$tmp/out"
[ ! -e "$(slot b)" ]
REMUDA_INSTALL_DIR="$tmp/bin"
export REMUDA_INSTALL_DIR

# A matching target slot cannot be selected when the old pair is unknown.
publish a
install_root="$tmp/unknown-slot"
mkdir -p "$install_root/.remuda-gui/builds"
cp -R "$tmp/bin/.remuda-gui/builds/build-$FAKE_VERSION+a" "$install_root/.remuda-gui/builds/"
printf '#!/bin/sh\nprintf called >"$FAKE_DIR/old-cli-called"\n' >"$install_root/remuda"
printf '#!/bin/sh\nprintf called >"$FAKE_DIR/old-host-called"\n' >"$install_root/remuda-session-host"
chmod +x "$install_root/remuda" "$install_root/remuda-session-host"
cp "$install_root/remuda" "$tmp/unknown-before"
rm -f "$tmp/old-host-called" "$tmp/old-cli-called"
REMUDA_INSTALL_DIR="$install_root"
export REMUDA_INSTALL_DIR
install_fails
same "$install_root/remuda" "$tmp/unknown-before"
[ ! -e "$tmp/old-host-called" ]
[ ! -e "$tmp/old-cli-called" ]
REMUDA_INSTALL_DIR="$tmp/bin"
export REMUDA_INSTALL_DIR

# Probe mismatch and an invalid checksum must fail before staging or core swap.
publish b
for role in --verify-cli --verify-host --verify; do
	FAKE_PROBE_FAIL=$role
	export FAKE_PROBE_FAIL
	install_fails
done
unset FAKE_PROBE_FAIL
[ ! -e "$(slot b)" ]
same "$tmp/bin/remuda" "$tmp/current"
rm -f "$tmp/probes"
printf '0000000000000000000000000000000000000000000000000000000000000000  remuda-%s-aarch64-apple-darwin.tar.gz\n' "$FAKE_VERSION" >"$tmp/SHA256SUMS"
install_fails
[ ! -e "$tmp/probes" ]
same "$tmp/bin/remuda" "$tmp/current"

# Partial formats and inconsistent metadata never start a probe.
publish f
rm "$tmp/pkg/remuda-gui.contract"
files="remuda remuda-session-host remuda-gui remuda-gui.build remuda-installer-probe"
pack
rm -f "$tmp/probes"
install_fails
[ ! -e "$tmp/probes" ]
same "$tmp/bin/remuda" "$tmp/current"
publish f
printf '%s\n' '9.9.9+wrong' >"$tmp/pkg/remuda-gui.build"
pack
rm -f "$tmp/probes"
install_fails
[ ! -e "$tmp/probes" ]
same "$tmp/bin/remuda" "$tmp/current"

# An incomplete slot and every managed path symlink fail without replacement.
publish c
mkdir -p "$(dirname "$(slot c)")"
install_fails
[ ! -e "$(slot c)" ]
same "$tmp/bin/remuda" "$tmp/current"
for partial in manifest-only gui-only; do
	publish "$partial"
	partial_slot=$(dirname "$(slot "$partial")")
	mkdir -p "$partial_slot"
	if [ "$partial" = manifest-only ]; then cp "$tmp/provenance" "$partial_slot/provenance"; else cp "$tmp/pkg/remuda-gui" "$partial_slot/remuda-gui"; fi
	install_fails
	if [ "$partial" = manifest-only ]; then same "$partial_slot/provenance" "$tmp/provenance"; else same "$partial_slot/remuda-gui" "$tmp/pkg/remuda-gui"; fi
	same "$tmp/bin/remuda" "$tmp/current"
done
publish g
mkdir -p "$(dirname "$(slot g)")"
cp "$tmp/pkg/remuda-gui" "$(slot g)"
chmod 644 "$(slot g)"
install_fails
same "$tmp/pkg/remuda-gui" "$(slot g)"
[ ! -x "$(slot g)" ]
same "$tmp/bin/remuda" "$tmp/current"
publish c
for part in store builds slot file manifest; do
	install_root="$tmp/link-$part"
	mkdir -p "$install_root" "$tmp/outside-$part"
	case "$part" in
	store) link="$install_root/.remuda-gui" ;;
	builds) mkdir "$install_root/.remuda-gui"; link="$install_root/.remuda-gui/builds" ;;
	slot) mkdir -p "$install_root/.remuda-gui/builds"; link="$install_root/.remuda-gui/builds/build-$FAKE_VERSION+c" ;;
	file) mkdir -p "$install_root/.remuda-gui/builds/build-$FAKE_VERSION+c"; link="$install_root/.remuda-gui/builds/build-$FAKE_VERSION+c/remuda-gui"
		manifest_file="$(dirname "$link")/provenance"
		printf 'remuda-gui-provenance-v1\nbuild %s+c\n' "$FAKE_VERSION" >"$manifest_file"
		for file in remuda remuda-session-host remuda-gui; do printf '%s %s\n' "$file" "$(sha256 "$tmp/pkg/$file" | awk '{print $1}')" >>"$manifest_file"; done ;;
	manifest) mkdir -p "$install_root/.remuda-gui/builds/build-$FAKE_VERSION+c"; link="$install_root/.remuda-gui/builds/build-$FAKE_VERSION+c/provenance" ;;
	esac
	ln -s "$tmp/outside-$part" "$link"
	REMUDA_INSTALL_DIR="$install_root"
	export REMUDA_INSTALL_DIR
	install_fails
	REMUDA_INSTALL_DIR="$tmp/bin"
	export REMUDA_INSTALL_DIR
	[ -L "$link" ] && [ -z "$(ls -A "$tmp/outside-$part")" ]
done

# Failure and forced parent death at publication never swap core binaries.
publish d
export FAKE_LINK_MODE=fail
install_fails
unset FAKE_LINK_MODE
same "$tmp/bin/remuda" "$tmp/current"
for phase in crash-manifest-before crash-manifest-after crash-before crash-after; do
	build="e-$phase"
	publish "$build"
	export FAKE_LINK_MODE="$phase"
	install_fails
	unset FAKE_LINK_MODE
	same "$tmp/bin/remuda" "$tmp/current"
	case "$phase" in
	crash-after) same "$tmp/pkg/remuda-gui" "$(slot "$build")" ;;
	*) [ ! -e "$(slot "$build")" ]; install_fails; same "$tmp/bin/remuda" "$tmp/current" ;;
	esac
done

# A later build keeps A; restoring the previous CLI/host pair keeps both GUIs.
publish b
install_ok
[ -f "$(slot a)" ] && [ -f "$(slot b)" ]
cp "$tmp/bin/remuda" "$tmp/build-b-current"
for file in remuda remuda-session-host; do
	publish a
	printf '# changed %s\n' "$file" >>"$tmp/pkg/$file"
	pack
	install_fails
	same "$tmp/bin/remuda" "$tmp/build-b-current"
done
cp "$tmp/bin/remuda.prev" "$tmp/bin/.rollback-remuda"
cp "$tmp/bin/remuda-session-host.prev" "$tmp/bin/.rollback-host"
mv "$tmp/bin/.rollback-host" "$tmp/bin/remuda-session-host"
mv "$tmp/bin/.rollback-remuda" "$tmp/bin/remuda"
same "$tmp/bin/remuda" "$tmp/current"
[ -f "$(slot a)" ] && [ -f "$(slot b)" ]

# Pre-GUI archive remains installable without removing a manual or managed GUI.
publish old old
install_ok
[ "$(cat "$tmp/bin/remuda-gui")" = 'manual GUI' ]
[ -f "$(slot a)" ]
[ "$(cat "$tmp/data/remuda/mods/gui/sentinel")" = 'existing mod' ]

# Optional macOS assembled-path test using real, identically built binaries.
if [ -n "${REMUDA_TEST_REAL_PROBE:-}" ]; then
	FAKE_VERSION=9.8.7
	REMUDA_INSTALL_DIR="$tmp/real-bin"
	export FAKE_VERSION REMUDA_INSTALL_DIR
	publish stage-proof.001
	cp "$REMUDA_TEST_REAL_PROBE" "$tmp/pkg/remuda-installer-probe"
	cp "$REMUDA_TEST_REAL_GUI" "$tmp/pkg/remuda-gui"
	cp "$REMUDA_TEST_REAL_CLI" "$tmp/pkg/remuda"
	cp "$REMUDA_TEST_REAL_HOST" "$tmp/pkg/remuda-session-host"
	pack
	XDG_DATA_HOME="$tmp/real-data"
	export XDG_DATA_HOME
	mkdir -p "$XDG_DATA_HOME/remuda/extensions/legacy"
	printf 'legacy data\n' >"$XDG_DATA_HOME/remuda/extensions/legacy/sentinel"
	install_ok
	same "$REMUDA_TEST_REAL_GUI" "$(slot stage-proof.001)"
	[ -f "$XDG_DATA_HOME/remuda/extensions/legacy/sentinel" ]
	[ ! -e "$XDG_DATA_HOME/remuda/mods" ]
	# The real `upgrade` command fetches this isolated script through curl.
	XDG_DATA_HOME="$tmp/upgrade-data"
	export XDG_DATA_HOME
	REMUDA_NO_UPDATE_CHECK=1 "$REMUDA_TEST_REAL_CLI" upgrade --channel nightly --no-reload >"$tmp/upgrade-out" 2>&1 || { cat "$tmp/upgrade-out"; exit 1; }
	same "$REMUDA_TEST_REAL_GUI" "$(slot stage-proof.001)"
	[ -f "$tmp/real-data/remuda/extensions/legacy/sentinel" ]
fi
echo 'ok — GUI staging preserves installed files and fails closed on conflicts'
