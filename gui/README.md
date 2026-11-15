# Remuda GUI

From the repository root, run the standalone GPUI prototype:

```sh
cargo run --locked --manifest-path gui/Cargo.toml
```

The GUI reads sessions from the existing default local daemon. It also lists
known cluster nodes; select an admitted peer to see its sessions, then select
an available session with a known instance ID to see a read-only text snapshot.
Local-only use does not require cluster initialization. Remote access uses the
existing cluster membership and authenticated peer transport.

Refresh updates the lists and clears the current preview. Offline or failed
requests are shown as stale or unavailable; the preview is a snapshot, not a
live terminal. Session creation, attachment, keyboard input, terminal styling,
scrollback navigation and IME are not available in this prototype.

The current prototype has been built and exercised on macOS. Other platforms
have not been validated for this GUI.

On macOS, `remuda gui` launches `remuda-gui` from the same directory as the
resolved `remuda` executable. The GUI is not yet included in release archives:
build it with `cargo build --release --locked --manifest-path gui/Cargo.toml`,
then place `gui/target/release/remuda-gui` beside the installed `remuda`.
The built-in GUI does not support named servers (`remuda -s NAME gui`) yet.
An installed mod declaring the `gui` command keeps precedence; in that case,
run the sibling `remuda-gui` executable directly to open this GUI.

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

# Worker compatibility

Read-only GUI workers accept legacy requests as well as ABI-tagged requests.
New GUI windows require a matching response envelope and show a restart notice
on an incompatible response. The input bridge requires its ready greeting
before sending bytes; input is not connected to the GUI yet. These markers
identify a protocol revision, not an exact executable build. Changes to either
protocol must update the marker and exercise both directions of compatibility.
An already-running pre-fence GUI cannot be retroactively protected; do not use
this mechanism as evidence that replacing its executable preserves its window.

To check actual pre-fence and current node-worker formats, build the GUI from
commit `76cd7fa`, then run from this checkout:

```sh
REMUDA_GUI_OLD_BINARY=/absolute/path/to/old/remuda-gui cargo test --locked --manifest-path gui/Cargo.toml --test worker_abi_transition -- --ignored
REMUDA_GUI_OLD_BINARY=/absolute/path/to/old/remuda-gui cargo test --locked --manifest-path gui/Cargo.toml --bin remuda-gui input_helper::tests::actual_legacy_input_worker_receives_no_payload -- --ignored --exact
```

These exercise real subprocesses against isolated empty cluster state and a
private fake daemon socket; they do not test an upgrade installer or input
delivery to a live session.
For the real-daemon local view supervisor above, setting
`REMUDA_GUI_EXPECT_STALE=1` and pointing `REMUDA_GUI_ACCEPTANCE_WORKER` to the
pre-fence binary checks that a new TestWindow rejects its legacy list response
and lays out the restart notice. Run the supervisor from the old checkout with
the new worker to check the opposite direction. Neither test replaces a running
executable or exercises a physical window.
