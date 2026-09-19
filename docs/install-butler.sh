#!/bin/sh
# Installs a persistence layer for `remuda exec butler`: a systemd --user
# timer (Linux) or a launchd agent (macOS) that polls `remuda ls` every 15s
# and re-runs `remuda exec butler` whenever no session is named exactly
# "butler" -- reviving the Matrix bridge after a daemon crash, `remuda
# restart`, or a reboot, none of which anything in `remuda` itself recovers
# from on its own (native/tests/daemon.rs,
# a_daemon_restart_does_not_relaunch_the_butler_session).
#
#   curl -fsSL https://warmblood-kr.github.io/remuda/install-butler.sh | sh
#
#   REMUDA_BUTLER_TOKEN_FILE=<path>   default: $XDG_CONFIG_HOME/remuda/butler/token
#   REMUDA_BUTLER_CONFIG_FILE=<path>  default: $XDG_CONFIG_HOME/remuda/butler/config
#
# This script only ever CONSUMES a token/config file placed there by some
# other means -- obtaining a Matrix access token is out of scope here, and
# this script never writes secret content anywhere.
#
# `remuda exec butler` is a one-shot registration call, not a long-running
# process (it returns as soon as the package is registered in the daemon's
# image -- see native/src/bin/remuda.rs's exec_command) -- so the persistence
# layer here is poll-and-relaunch, not Restart=on-failure/KeepAlive: wrapping
# a call that always exits 0 in either would just relaunch it in a tight loop
# without ever noticing the daemon underneath had died.
#
# Windows has no systemd/launchd equivalent wired up here yet: run `remuda
# exec butler` by hand after a restart, or via Task Scheduler, until someone
# builds that lane.
#
# The reboot leg is unproven, by construction: the daemon-restart/relaunch
# logic below was verified for real (`restart -f`, a genuine registration,
# a kill, a by-hand relaunch matching the poll loop), but never across an
# actual `reboot` -- this was developed on the shared fleet substrate, where
# rebooting would kill every concurrent session on it. Whether OnBootSec=10s
# actually fires after a real reboot, and whether `loginctl enable-linger`
# actually keeps the --user instance alive through a real logout/reboot
# cycle, needs a machine a human can reboot on purpose. Open limitation, not
# a verified claim.
#
# Ceiling, recorded not fixed: `init.lua` no longer depends on the daemon's
# birth environment for the token/config *paths* (this script's own copy step
# below plus init.lua's conventional-path default is what makes that true),
# but every OTHER environment variable the daemon's Lua image reads via
# `os.getenv` still has that same birth-environment property -- fixed forever
# at whichever moment first auto-started the daemon, never re-read per
# client/request. That's a `remuda`-architecture-level question, not this
# feature's to solve; naming it here so it isn't rediscovered as a surprise.

set -eu

die() {
	echo "install-butler.sh: $*" >&2
	exit 1
}

status() {
	echo "install-butler.sh: $*" >&2
}

os=$(uname -s)
case "$os" in
Linux | Darwin) ;;
*) die "no persistence layer for $os yet (only Linux/systemd and macOS/launchd are wired up) -- run 'remuda exec butler' by hand after every restart" ;;
esac

config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
token_file="${REMUDA_BUTLER_TOKEN_FILE:-$config_home/remuda/butler/token}"
config_file="${REMUDA_BUTLER_CONFIG_FILE:-$config_home/remuda/butler/config}"

[ -f "$token_file" ] || die "no token file at $token_file -- place the Matrix access token there first (this script does not obtain or create one)"
[ -f "$config_file" ] || die "no config file at $config_file -- place homeserver/room-id/self-mxid/allowed-senders (one per line) there first (this script does not create one)"

# Assert the mode landed, rather than trusting chmod's exit status alone --
# a read-only filesystem or a chmod that silently no-ops is exactly the case
# this exists to catch.
chmod 600 "$token_file"
if command -v stat >/dev/null 2>&1 && stat -c '%a' "$token_file" >/dev/null 2>&1; then
	token_mode=$(stat -c '%a' "$token_file")
else
	token_mode=$(stat -f '%Lp' "$token_file")
fi
[ "$token_mode" = 600 ] || die "chmod 600 on $token_file did not take (mode is now $token_mode) -- refusing to proceed with a wider-than-600 token file"

# `init.lua`'s own default lookup (no env override) is the fixed path
# $XDG_CONFIG_HOME/remuda/butler/{token,config} -- copy the resolved files
# there if an override pointed elsewhere, so the daemon's default resolves
# regardless of what/when first birthed it (no butler env var required in
# the persistent unit below). A no-op when the override already matches the
# default -- comparing paths first instead of always copying.
canonical_token_file="$config_home/remuda/butler/token"
canonical_config_file="$config_home/remuda/butler/config"
if [ "$token_file" != "$canonical_token_file" ]; then
	mkdir -p "$(dirname "$canonical_token_file")"
	cp "$token_file" "$canonical_token_file"
	chmod 600 "$canonical_token_file"
	status "copied $token_file to $canonical_token_file (init.lua's default lookup)"
fi
if [ "$config_file" != "$canonical_config_file" ]; then
	mkdir -p "$(dirname "$canonical_config_file")"
	cp "$config_file" "$canonical_config_file"
	chmod 600 "$canonical_config_file"
	status "copied $config_file to $canonical_config_file (init.lua's default lookup)"
fi

remuda_bin=$(command -v remuda) || die "remuda is not on PATH -- install it first: curl -fsSL https://warmblood-kr.github.io/remuda/install.sh | sh"
remuda_bin_dir=$(dirname "$remuda_bin")

# A real functional probe, not a version-string parse: reuse the exact error
# text the binary already produces (`no such package: butler`) rather than
# hardcoding a date or commit that will rot the moment the package is
# renamed or the check is run against a future release scheme.
status "registering butler in the running daemon (this starts one if none is up)..."
probe_err=$(mktemp)
trap 'rm -f "$probe_err"' EXIT INT TERM
if ! env -u PWD REMUDA_BUTLER_TOKEN="$token_file" REMUDA_BUTLER_CONFIG="$config_file" remuda exec butler 2>"$probe_err"; then
	if grep -q 'no such package: butler' "$probe_err"; then
		die "this remuda build predates the butler package -- run 'remuda upgrade', then re-run this installer"
	fi
	cat "$probe_err" >&2
	die "remuda exec butler failed -- see the error above; not installing any persistence layer"
fi

# Exit 0 here only proves the daemon didn't reject the package name -- it does
# NOT prove butler did anything. Between the `exec` verb landing and the real
# butler package replacing its old 2-line stub, `remuda exec butler` already
# exited 0 with empty stderr while registering no process, tool, or session at
# all. Assert the positive post-condition instead of trusting the exit code.
#
# This makes the exact-match `remuda ls` check the THIRD consumer of one
# liveness instrument, alongside butler-poll.sh below and
# native/tests/daemon.rs's a_daemon_restart_does_not_relaunch_the_butler_session.
# That repetition is only safe because the daemon.rs test is a negative
# control on `remuda ls` itself -- if `remuda ls` ever lied about a session's
# presence, that test goes red. Weakening or deleting it re-enables a known
# false-positive across all three consumers, not just loosens one test.
if ! env -u PWD remuda ls | awk '$1 == "butler" { found = 1 } END { exit !found }'; then
	die "remuda exec butler exited successfully but registered no session named 'butler' -- this remuda build is between the 'exec' verb landing and the real butler package landing (a real but narrow window); run 'remuda upgrade' and try again"
fi
status "butler registered for this run."

# The poll-and-relaunch logic lives in its own small script rather than
# inline in the unit/plist ExecStart -- both systemd unit files and plist
# argv strings have their own quoting rules for '$', '"' and '%', and a
# five-line shell script sidesteps all of that instead of fighting it.
#
# Exact first-field match on `remuda ls`, never a substring: a substring
# match (e.g. `grep -q butler`) would also match a session named
# `butler-test` or `my-butler-thing` and silently never relaunch. See
# native/tests/daemon.rs's exact-match discussion.
remuda_dir="$config_home/remuda"
mkdir -p "$remuda_dir"
poll_script="$remuda_dir/butler-poll.sh"
cat >"$poll_script" <<POLL
#!/bin/sh
# Written by install-butler.sh. No REMUDA_BUTLER_TOKEN/REMUDA_BUTLER_CONFIG
# here (and none in the systemd unit / launchd plist that run this) --
# init.lua's own default lookup finds the token/config files this script
# already copied to the conventional path above, no matter what first
# birthed the daemon.
set -eu
export PATH="$remuda_bin_dir:\$PATH"
env -u PWD remuda ls | awk '\$1 == "butler" { found = 1 } END { exit !found }' && exit 0
exec env -u PWD remuda exec butler
POLL
chmod 755 "$poll_script"
status "wrote $poll_script"

case "$os" in
Linux)
	unit_dir="$HOME/.config/systemd/user"
	mkdir -p "$unit_dir"
	service="$unit_dir/remuda-butler.service"
	timer="$unit_dir/remuda-butler.timer"

	cat >"$service" <<EOF
[Unit]
Description=remuda-butler: relaunch the butler session if it is not running

[Service]
Type=oneshot
ExecStart=%h/.config/remuda/butler-poll.sh
EOF

	# No [Install] here -- this unit is triggered by the timer below, never
	# enabled for a target on its own.
	cat >"$timer" <<'EOF'
[Unit]
Description=Poll for the remuda butler session and relaunch it if missing

[Timer]
OnBootSec=10s
OnUnitActiveSec=15s

[Install]
WantedBy=timers.target
EOF

	status "wrote $service"
	status "wrote $timer"
	systemctl --user daemon-reload
	status "daemon-reload done. Two more steps, run these yourself:"
	status ""
	status "  systemctl --user enable --now remuda-butler.timer"
	status ""
	status "  loginctl enable-linger \"\$(whoami)\""
	status ""
	status "The second one matters for the case nobody is logged in yet after a"
	status "reboot: without linger, systemd tears down your --user instance (and"
	status "this timer with it) the moment your last session ends. On a machine"
	status "where polkit restricts who can set linger, that command may fail --"
	status "that's a normal failure for you to see and act on, not something"
	status "this script tries to detect or paper over."
	;;
Darwin)
	agent_dir="$HOME/Library/LaunchAgents"
	mkdir -p "$agent_dir"
	plist="$agent_dir/kr.warmblood.remuda.butler.plist"

	cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>kr.warmblood.remuda.butler</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/sh</string>
		<string>$poll_script</string>
	</array>
	<key>StartInterval</key>
	<integer>15</integer>
</dict>
</plist>
EOF

	status "wrote $plist"
	status ""
	status "StartInterval (not RunAtLoad+KeepAlive) on purpose: the poll script"
	status "always exits 0 quickly, so KeepAlive would relaunch it in a tight"
	status "loop instead of waiting for the next interval."
	status ""
	status "One more step, run this yourself:"
	status ""
	status "  launchctl bootstrap gui/\$(id -u) $plist"
	;;
esac
