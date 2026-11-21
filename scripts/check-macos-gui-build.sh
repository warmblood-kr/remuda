#!/bin/sh
# Nonpublishing build, archive and isolated installer proof for macOS arm64.
set -eu
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || { echo 'requires macOS arm64' >&2; exit 1; }
root=$(cd "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT INT TERM
export REMUDA_VERSION=9.8.7 REMUDA_BUILD=stage-proof.001
build_id="$REMUDA_VERSION+$REMUDA_BUILD"
printf 'Expected build: %s\n' "$build_id"
target=aarch64-apple-darwin
if [ -n "${REMUDA_GUI_PROOF_BIN_DIR:-}" ]; then
	bin_dir=$REMUDA_GUI_PROOF_BIN_DIR
	echo 'Using prebuilt binaries: this does not prove a clean release build.'
else
	CARGO_TARGET_DIR="$root/target" cargo build --locked --release --target "$target" \
		-p remuda-native --features installer-probe-experimental \
		--bin remuda --bin remuda-session-host --bin remuda-installer-probe
	CARGO_TARGET_DIR="$root/target" cargo build --locked --release --target "$target" \
		--manifest-path "$root/gui/Cargo.toml" --bin remuda-gui
	bin_dir="$root/target/$target/release"
fi

mkdir -p "$scratch/bundle" "$scratch/shim" "$scratch/data" "$scratch/bin"
for file in remuda remuda-session-host remuda-gui remuda-installer-probe; do
	[ -f "$bin_dir/$file" ] && [ -x "$bin_dir/$file" ] || { echo "missing executable $file" >&2; exit 1; }
	[ "$(lipo -archs "$bin_dir/$file")" = arm64 ] || { echo "$file is not arm64-only" >&2; exit 1; }
	cp "$bin_dir/$file" "$scratch/bundle/$file"
done
printf '%s\n' remuda-gui-archive-v1 >"$scratch/bundle/remuda-gui.contract"
printf '%s\n' "$build_id" >"$scratch/bundle/remuda-gui.build"
probe="$scratch/bundle/remuda-installer-probe"
export REMUDA_RUNTIME_DIR="$scratch/runtime" XDG_CONFIG_HOME="$scratch/config"
export XDG_DATA_HOME="$scratch/data" XDG_STATE_HOME="$scratch/state" XDG_CACHE_HOME="$scratch/cache"
export REMUDA_NO_UPDATE_CHECK=1
for spec in '--verify-cli:remuda' '--verify-host:remuda-session-host' '--verify:remuda-gui'; do
	mode=${spec%%:*}
	file=${spec#*:}
	"$probe" "$mode" --verified-archive-v1 "$scratch/bundle/$file" "$build_id"
done
if "$probe" --verify --verified-archive-v1 "$scratch/bundle/remuda-gui" "$build_id-wrong" 2>"$scratch/mismatch"; then
	echo 'GUI build mismatch unexpectedly accepted' >&2
	exit 1
fi
echo 'GUI build mismatch rejected as expected'

# This archive remains local to the job; it is not a release artifact.
archive="remuda-$REMUDA_VERSION-$target.tar.gz"
tar -czf "$scratch/$archive" -C "$scratch/bundle" remuda remuda-session-host remuda-gui \
	remuda-installer-probe remuda-gui.contract remuda-gui.build
tar -tzf "$scratch/$archive" | LC_ALL=C sort >"$scratch/actual"
printf '%s\n' remuda remuda-session-host remuda-gui remuda-installer-probe \
	remuda-gui.contract remuda-gui.build | LC_ALL=C sort >"$scratch/expected"
cmp -s "$scratch/actual" "$scratch/expected" || { echo 'archive inventory mismatch' >&2; exit 1; }
(cd "$scratch" && shasum -a 256 "$archive" >SHA256SUMS)
printf '{"stable":"0.0.0","nightly":"%s"}\n' "$REMUDA_VERSION" >"$scratch/latest.json"
cat >"$scratch/shim/curl" <<'SH'
#!/bin/sh
for arg in "$@"; do url=$arg; done
case "$url" in
  */latest.json) cat "$REMUDA_GUI_PROOF_RELEASE/latest.json" ;;
  */SHA256SUMS) cat "$REMUDA_GUI_PROOF_RELEASE/SHA256SUMS" ;;
  *) cat "$REMUDA_GUI_PROOF_RELEASE/$REMUDA_GUI_PROOF_ARCHIVE" ;;
esac
SH
chmod +x "$scratch/shim/curl"
export REMUDA_GUI_PROOF_RELEASE="$scratch" REMUDA_GUI_PROOF_ARCHIVE="$archive"
export REMUDA_INSTALL_DIR="$scratch/bin" REMUDA_CHANNEL=nightly PATH="$scratch/shim:$PATH"
sh "$root/docs/install.sh"
installed="$REMUDA_INSTALL_DIR/.remuda-gui/builds/build-$build_id/remuda-gui"
cmp -s "$scratch/bundle/remuda-gui" "$installed" || { echo 'installed GUI differs' >&2; exit 1; }
"$probe" --verify --verified-archive-v1 "$installed" "$build_id"
printf 'Verified local archive contents:\n'
cat "$scratch/actual"
cat "$scratch/SHA256SUMS"
for file in remuda remuda-session-host remuda-gui remuda-installer-probe; do
	printf '%s bytes: ' "$file"
	wc -c <"$scratch/bundle/$file"
	(cd "$scratch/bundle" && shasum -a 256 "$file")
done
