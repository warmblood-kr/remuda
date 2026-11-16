# Remuda GUI

From the repository root, build and run the standalone GPUI prototype:

```sh
cargo build --locked --manifest-path gui/Cargo.toml --bin remuda-gui
./gui/target/debug/remuda-gui
```

Run the binary directly so it inherits the intended node's state directory.
This repository's Cargo configuration forces a separate test state directory
for `cargo run`; a custom Cargo target directory changes the binary path.

The GUI reads sessions from the existing default local daemon. It also lists
known cluster nodes; select an admitted peer to see its sessions, then select
an available session with a known instance ID to see its read-only live text screen.
Local-only use does not require cluster initialization. Remote access uses the
existing cluster membership and authenticated peer transport.

Refresh updates the lists and clears the current preview. Offline or failed
requests are shown as stale or unavailable; the preview is a snapshot, not a
live terminal. For a current local session, click the line input and type an
ASCII line, then press Enter to send it. The selected admitted peer also has a
line input: it identifies the full fingerprint, session and instance, checks
protocol support, and sends one guarded line. A line can append to text already
at the remote prompt. On this revision admitted peers may send input regardless
of the advisory per-session opt-out or the node's remote-control setting. For
a selected local session, bounded
versioned Sync requests update its text snapshot as output changes. If Sync
fails, the GUI clears the snapshot and retries with a delay. The same bounded,
instance-checked Sync path refreshes an admitted peer's selected screen; peer
membership and pin are rechecked around each request. This read-only observation
checks the session instance but does not grant input or attachment.
Only one line can be in flight, including across selection changes. A failure
can leave delivery uncertain; the GUI identifies the original session and
disables further input. Check it before restarting the GUI. This uses
instance-bound input and does not attach or reserve the session. Session
creation and interactive peer terminal attachment are not available in this
prototype.

Select a current local session and choose **Attach interactive terminal** to
use the existing attach client inside the GUI. Attach takes over an existing
terminal holder. The worker checks the selected instance immediately before
attaching, but the existing attach protocol addresses the session by name: a
same-name recreation in between may attach the replacement. Selecting another
session, refreshing, closing the GUI, or choosing **Detach terminal** ends this
GUI's attach. The terminal currently displays an 80×24 plain-text screen and
supports ordinary text, Enter, arrows and common control keys. Color styling,
IME, scrollback, mouse reporting and automatic resize are not yet supported.

The current prototype has been built and exercised on macOS. Other platforms
have not been validated for this GUI.

The internal `remuda-gui --internal-build-id` probe prints the build identifier
embedded by `remuda-native` and exits before opening a window or reading state.
It identifies a build but does not authenticate the executable. Older GUI
versions do not support this probe and may open a window for an unknown option;
callers must establish compatibility and apply a timeout before invoking it.
To test the build-ID contract against real CLI and host binaries, build them
with the same `REMUDA_VERSION` and `REMUDA_BUILD` used for the GUI, then run:

```sh
REMUDA_GUI_BUILD_CLI=/absolute/path/to/remuda REMUDA_GUI_BUILD_HOST=/absolute/path/to/remuda-session-host cargo test --locked --manifest-path gui/Cargo.toml --test build_id gui_cli_and_host_identify_the_same_build -- --ignored --exact
```

Pass those same version/build variables to this test's Cargo invocation too.

On macOS, `remuda gui` launches `remuda-gui` from the same directory as the
resolved `remuda` executable. The GUI is not yet included in release archives:
build it with `cargo build --release --locked --manifest-path gui/Cargo.toml`,
then place `gui/target/release/remuda-gui` beside the installed `remuda`.
The built-in GUI does not support named servers (`remuda -s NAME gui`) yet.
An installed mod declaring the `gui` command keeps precedence; in that case,
run the sibling `remuda-gui` executable directly to open this GUI.

The launcher can also use a managed GUI at
`<install-dir>/.remuda-gui/builds/build-<build>/remuda-gui`, where `<build>` is
the complete build string printed after `remuda ` by `remuda --version`. A
manual `remuda-gui` sibling takes precedence. Managed build entries must never
be modified or removed while a window may be running; the launcher starts the
build-specific path so that window's helper processes keep the same executable.
No installer currently creates or prunes this layout. A window opened earlier
from a mutable sibling is not retroactively pinned by this launcher.
Selection checks the existing path components, but does not protect against
another process replacing them concurrently; future installation must keep
managed entries stable, including while a GUI is starting or running.

To run the macOS launcher rollback test, build `remuda` twice from the same
checkout with distinct `REMUDA_BUILD` values, save both binaries, and run:

```sh
REMUDA_GUI_CLI_A=/absolute/path/to/first/remuda REMUDA_GUI_CLI_B=/absolute/path/to/second/remuda cargo test --locked -p remuda-native --test gui_launcher macos::actual_distinct_builds_preserve_running_gui_and_rollback -- --ignored --exact
```

The test starts executable probes at both managed paths and swaps only files
inside its disposable directory; it does not test an installer or GPUI window.

# Peer-list acceptance

Run `sh gui/tests/peer_list_acceptance.sh` from the repository. It builds the
GUI unless `REMUDA_GUI_ACCEPTANCE_BIN` names an executable already built from
this checkout. `REMUDA_GUI_ACCEPTANCE_TARGET` changes the GUI build directory.
The ignored integration test starts isolated local daemons and uses loopback;
it checks the authenticated peer worker, session instance, invalid target and
offline peer. It does not exercise a real GPUI window or a physical network.

# Peer live view and input acceptance

From an isolated checkout on macOS, build the CLI and GUI, then build the GUI
unit-test executable without running it:

```sh
cargo build --locked -p remuda-native --bin remuda
cargo build --locked --manifest-path gui/Cargo.toml --bin remuda-gui
cargo test --locked --manifest-path gui/Cargo.toml --bin remuda-gui --no-run
REMUDA_GUI_ACCEPTANCE_REMUDA="$PWD/target/debug/remuda" \
REMUDA_GUI_PEER_TEST_BIN="ABSOLUTE_TEST_EXECUTABLE_PRINTED_ABOVE" \
python3 gui/tests/peer_live_smoke.py
```

The smoke creates two temporary XDG states and private runtimes without
changing HOME, binds one listener to loopback, creates an automatically writing
test session and cleans up its own daemons. It exercises actual GUI TestWindow
node/session clicks through pinned peer Sync to two output frames, and rejects
wrong-instance, unadmitted and revoked peers for viewing. It sends a line only
to its own disposable peer shell and takes no terminal attachment. Physical keyboard events and pixels are not tested.

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
session reaches the GPUI model, its row is laid out, and a simulated click and
line submission produce output from the isolated shell. TestWindow does not
verify physical OS keyboard events or screen pixels. The same acceptance also
clicks Attach, types a command through the embedded PTY, checks its shell output,
and verifies that changing selection releases the attach. The normal GUI test suite
includes cancellation and failure-path checks. Commands run in separate
sessions and cleanup signals only their owned process groups; a descendant that
creates a new session is outside that scope.

# Worker compatibility

Read-only GUI workers accept legacy requests as well as ABI-tagged requests.
New GUI windows require a matching response envelope and show a restart notice
on an incompatible response. The input bridge requires its version 2 ready greeting before sending bytes;
version 1 workers are refused without receiving input because peer line semantics
changed. Local and admitted-peer line input are connected to the GUI. These markers
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
