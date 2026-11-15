# Local session list

`cargo run --manifest-path gui/Cargo.toml` opens a read-only list from the existing default local daemon. No cluster initialization, attachment, or input is performed. Refresh updates the list; an offline result retains and marks old rows stale.
# Peer-list acceptance

Run `sh gui/tests/peer_list_acceptance.sh` from the repository. It builds the
GUI unless `REMUDA_GUI_ACCEPTANCE_BIN` names an executable already built from
this checkout. `REMUDA_GUI_ACCEPTANCE_TARGET` changes the GUI build directory.
The ignored integration test starts isolated local daemons and uses loopback;
it checks the authenticated peer worker, session instance, invalid target and
offline peer. It does not exercise a real GPUI window or a physical network.

# Local view acceptance

On macOS, to run the explicitly ignored real-daemon GPUI acceptance test, build
from the same checkout (adjust paths for `CARGO_TARGET_DIR`):

```sh
cargo build -p remuda-native --bin remuda
cargo build --manifest-path gui/Cargo.toml --bin remuda-gui
cargo build --manifest-path gui/Cargo.toml --example real_local_view
REMUDA_GUI_ACCEPTANCE_REMUDA="$PWD/target/debug/remuda" REMUDA_GUI_ACCEPTANCE_WORKER="$PWD/gui/target/debug/remuda-gui" "$PWD/gui/target/debug/examples/real_local_view"
```

The supervisor uses temporary runtime, config, data and state directories, starts
an in-process-backend local daemon, and stops it on exit. It verifies an actual
session reaches the GPUI model and its row is laid out; TestWindow does not
assert physical window pixels, fonts or OS input. The normal GUI test suite
includes cancellation and failure-path checks. Commands run in separate
sessions and cleanup signals only their owned process groups; a descendant that
creates a new session is outside that scope.
