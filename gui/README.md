# Local session list

`cargo run --manifest-path gui/Cargo.toml` opens a read-only list from the existing default local daemon. No cluster initialization, attachment, or input is performed. Refresh updates the list; an offline result retains and marks old rows stale.
# Peer-list acceptance

Run `sh gui/tests/peer_list_acceptance.sh` from the repository. It builds the
GUI unless `REMUDA_GUI_ACCEPTANCE_BIN` names an executable already built from
this checkout. `REMUDA_GUI_ACCEPTANCE_TARGET` changes the GUI build directory.
The ignored integration test starts isolated local daemons and uses loopback;
it checks the authenticated peer worker, session instance, invalid target and
offline peer. It does not exercise a real GPUI window or a physical network.
