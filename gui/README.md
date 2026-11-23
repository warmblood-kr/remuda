# Remuda GUI

On Apple silicon macOS and x64 Windows, the [nightly installer](../README.md#install-the-browser-nightly)
installs the CLI and the matching native GUI together. Run
`remuda browser --gui HOST` with an SSH host or alias for a machine running Remuda.
The native Browser opens the remote Remuda terminal and can receive
[VFS document-open requests](../README.md#open-a-vfs-document-for-a-browser-reader)
in the same window. Browser mode does not need a local daemon or cluster membership.

## Local session prototype

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

Use `remuda gui` on macOS or Windows to open this view. With a configured
cluster, choose an admitted node to browse its sessions. Without a cluster,
the view shows your local sessions.

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
resolved `remuda` executable. To build and install the GUI manually, run
`cargo build --release --locked --manifest-path gui/Cargo.toml`,
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
The nightly installer creates these entries. A window opened earlier
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
