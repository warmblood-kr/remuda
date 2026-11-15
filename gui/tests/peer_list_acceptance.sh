#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
cd "$repo"
cargo_bin=${REMUDA_GUI_ACCEPTANCE_CARGO:-cargo}
if [ -z "${REMUDA_GUI_ACCEPTANCE_BIN:-}" ]; then
    target=${REMUDA_GUI_ACCEPTANCE_TARGET:-"$repo/gui/target"}
    case "$target" in /*) ;; *) target="$repo/$target" ;; esac
    "$cargo_bin" build --locked --manifest-path gui/Cargo.toml --target-dir "$target"
    REMUDA_GUI_ACCEPTANCE_BIN="$target/debug/remuda-gui"
fi
case "$REMUDA_GUI_ACCEPTANCE_BIN" in
    /*) ;;
    *) REMUDA_GUI_ACCEPTANCE_BIN="$repo/$REMUDA_GUI_ACCEPTANCE_BIN" ;;
esac
test -x "$REMUDA_GUI_ACCEPTANCE_BIN"
target=${REMUDA_GUI_ACCEPTANCE_TARGET:-"$repo/gui/target"}
case "$target" in /*) ;; *) target="$repo/$target" ;; esac
"$cargo_bin" build --locked --manifest-path gui/Cargo.toml --target-dir "$target" --example input_bridge_acceptance
REMUDA_GUI_BRIDGE_ACCEPTANCE_BIN="$target/debug/examples/input_bridge_acceptance"
export REMUDA_GUI_ACCEPTANCE_BIN REMUDA_GUI_BRIDGE_ACCEPTANCE_BIN
"$cargo_bin" test -p remuda-native --test cluster_remote \
    gui_peer_list_worker_reads_pinned_peer_and_reports_offline -- --ignored --exact --nocapture
