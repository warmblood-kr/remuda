#!/usr/bin/env bash
# Fail when Python enters the core tree outside the temporary, owned port list.
set -euo pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"
export LC_ALL=C
inventory=$(mktemp)
trap 'rm -f "$inventory"' EXIT HUP INT TERM
git ls-files --cached --others --exclude-standard -z >"$inventory" || {
	echo 'check-no-python: cannot enumerate repository files' >&2
	exit 1
}

# Keep one row per exception. The six CI checkers are assigned to core CI.
ALLOWLIST=$(cat <<'EOF'
scripts/check-principles.py|owner: remuda-dev-lead|TODO: port to Rust, see #289
scripts/check-steps.py|owner: remuda-dev-lead|TODO: port to Rust, see #289
scripts/check-comments.py|owner: remuda-dev-lead|TODO: port to Rust, see #289
scripts/check-install.py|owner: remuda-dev-lead|TODO: port to Rust, see #289
scripts/check-butler-path-convention.py|owner: remuda-dev-lead|TODO: port to Rust, see #289
scripts/check-workflows.py|owner: remuda-dev-lead|TODO: port to Rust, see #289
EOF
)

row_count=$(printf '%s\n' "$ALLOWLIST" | wc -l | tr -d ' ')
[ "$row_count" -eq 6 ] || {
	echo "check-no-python: allowlist must have exactly six documented rows" >&2
	exit 1
}
for expected in \
	scripts/check-principles.py \
	scripts/check-steps.py \
	scripts/check-comments.py \
	scripts/check-install.py \
	scripts/check-butler-path-convention.py \
	scripts/check-workflows.py; do
	count=$(printf '%s\n' "$ALLOWLIST" | awk -F '|' -v path="$expected" '$1 == path { count++ } END { print count + 0 }')
	[ "$count" -eq 1 ] || {
		echo "check-no-python: allowlist must contain exactly one row for $expected" >&2
		exit 1
	}
done

is_allowed_py() {
	printf '%s\n' "$ALLOWLIST" | while IFS='|' read -r path owner todo; do
		[ "$path" = "$1" ] && exit 0
	done
}

# Split policy tokens so the guard scans its own source without an exemption.
python_token='py''thon'
pip_token='pi''p'
uv_token='u''v'
setup_token='setup-py''thon'
policy_matches() {
	local status=0
	grep "$@" || status=$?
	case "$status" in
		0|1) return "$status" ;;
		*)
			echo "check-no-python: cannot match repository policy for: $file" >&2
			exit 1
			;;
	esac
}
failed=0
while IFS= read -r -d '' file; do
	if [ -L "$file" ] || [ ! -f "$file" ]; then
		echo "check-no-python: cannot inspect regular repository file: $file" >&2
		failed=1
		continue
	fi
	if is_allowed_py "$file"; then continue; fi
	first_line=$(head -n 1 -- "$file" | tr -d '\000') || {
		echo "check-no-python: cannot read repository file: $file" >&2
		exit 1
	}
	# Extensions in any case, packaging files, and Python shebangs anywhere.
	if policy_matches -qiE '\.pyw?$' <<<"$file"; then
		echo "check-no-python: unexpected Python file: $file" >&2
		failed=1
	elif policy_matches -qiE '^requirements.*\.(txt|in)$' <<<"${file##*/}"; then
		echo "check-no-python: unexpected Python requirements file: $file" >&2
		failed=1
	elif policy_matches -qE '^#!.*py''thon' <<<"$first_line"; then
		echo "check-no-python: unexpected Python shebang: $file" >&2
		failed=1
	fi
done <"$inventory"

while IFS='|' read -r file owner todo; do
	[ -n "$file" ] || continue
	[ -f "$file" ] || {
		echo "check-no-python: allowlisted file is missing: $file" >&2
		failed=1
	}
	case "$file" in
		scripts/check-principles.py|scripts/check-steps.py|scripts/check-comments.py|\
		scripts/check-install.py|scripts/check-butler-path-convention.py|scripts/check-workflows.py)
			expected_owner='owner: remuda-dev-lead'
			expected_todo='TODO: port to Rust, see #289'
			;;
		*)
			echo "check-no-python: unexpected allowlist row: $file|$owner|$todo" >&2
			failed=1
			;;
	esac
	if [ "$owner" != "$expected_owner" ] || [ "$todo" != "$expected_todo" ]; then
		echo "check-no-python: wrong owner or TODO for allowlisted file: $file" >&2
		failed=1
	fi
done <<EOF
$ALLOWLIST
EOF

# Text guard limit: a name built at run time (p=py""thon3; "$p") is not seen.
# A token is execution only in command position or next to an execution sink.
# A quoted token elsewhere is data, which keeps language-tag literals
# (language: Some("python"), tokens(line, "python"), a ```python fence) out of
# the report. The prefix is a path component list, so a word like `no-python`
# or `python_token` is not a command word.
scan_invocations() {
	awk -v tok="$python_token" -v piptok="$pip_token" -v uvtok="$uv_token" \
		-v setup="$setup_token" '
	BEGIN {
		tokonly = "(" tok "[0-9]*(\\.[0-9]+)*(\\.[eE][xX][eE])?|" piptok "[0-9]*((\\.[0-9]+)|\\.exe)?|" uvtok "(\\.exe)?)"
		tokre = "([^A-Za-z0-9_.-]|^)" tokonly "([^A-Za-z0-9_.-]|$)"
		sink = "(command::new|process::command|subprocess\\.|os\\.(system|exec|spawn|popen)|popen|system\\(|exec\\(|spawn\\(|(sh|bash|zsh|dash|ksh)[ \t]+-[A-Za-z]*c|eval[ \t]+|[|][ \t]*((env|command|exec|sudo)[ \t]+)*(sh|bash|zsh|dash|ksh)([ \t]|$))"
		executor = "(^|[;|&{}]|\\$\\(|`|run:[ \t]*)[ \t]*((sudo|doas|pkexec|env|exec|command|nohup|nice|setsid|stdbuf|timeout|ssh|xargs|watch|systemd-run|strace|ltrace|busybox|chroot|nsenter|osascript|perl|ruby|node|php|lua|docker|docker-compose|podman|kubectl)([ \t]+|$)|gdb[ \t]+--args|-exec[ \t])"
		setupre = "([^A-Za-z0-9_.-]|^)" setup "([^A-Za-z0-9_.-]|$)"
		runner = "(sudo|env|exec|xargs|command|nohup|nice|setsid|stdbuf|timeout|chrt|ionice|time|if|then|else|do|while|until)"
		args = "((-[^ \t]+([ \t]+[A-Za-z0-9_.=&/-]+)?|[0-9]+|[A-Za-z_][A-Za-z0-9_]*=[^ \t]*)[ \t]+)*"
		path = "(\\.{0,2}/)?([A-Za-z0-9_.-]+/)*"
		sep = "(^|[;|&{}]|\\$\\(|`|run:[ \t]*|(-exec|-execdir)[ \t]+)"
		cmd = sep "[ \t]*(" runner "[ \t]+" args ")?" path tokonly "([^A-Za-z0-9_.-]|$)"
		qcmd = sep "[ \t]*[\"\047]([^\"\047]*/)?" tokonly "([\"\047]|[ \t])"
		arm = sep "[ \t]*[\"\047][^\"\047]*" tokonly "[^\"\047]*[\"\047][ \t]*(\\||=>)"
		word = "([^A-Za-z0-9_.-]|^)" tokonly "([^A-Za-z0-9_.-]|$)"
		# A command substitution inside double quotes still executes, and
		# blank_quotes keeps it; a backtick substitution opens right after the
		# double quote (the markdown fence `"```python` has an empty first pair).
		btq = "\"[^\"`]*`[^`]*" tokonly
	}
	# Blank quoted text in place, preserving length, so an unquoted token is
	# still seen next to a quote while a literal inside one is not. A $(...)
	# inside double quotes is command substitution, so its content stays
	# visible even though the surrounding string does not.
	function blank_quotes(s,   i, c, q, cmdsub, depth, subq, out) {
		out = ""
		q = ""
		cmdsub = 0
		depth = 0
		subq = ""
		for (i = 1; i <= length(s); i++) {
			c = substr(s, i, 1)
			if (cmdsub) {
				if (subq == "\047") {
					out = out c
					if (c == "\047") subq = ""
					continue
				}
				if (c == "\\" && i < length(s)) {
					i++
					out = out c substr(s, i, 1)
					continue
				}
				if (subq == "\"") {
					out = out c
					if (c == "\"") subq = ""
					continue
				}
				if (c == "\047") { subq = "\047"; out = out c; continue }
				if (c == "\"") { subq = "\""; out = out c; continue }
				if (c == "(") depth++
				else if (c == ")") {
					depth--
					if (depth == 0) cmdsub = 0
				}
				out = out c
				continue
			}
			if (q == "") {
				if (c == "\"" || c == "\047") { q = c; out = out " " }
				else out = out c
			} else if (q == "\047") {
				if (c == "\047") q = ""
				out = out " "
			} else if (c == "\\" && i < length(s)) {
				out = out "  "
				i++
			} else if (c == "\"") {
				q = ""
				out = out " "
			} else if (c == "$" && substr(s, i + 1, 1) == "(") {
				cmdsub = 1
				depth = 1
				out = out "("
				i++
			} else {
				out = out " "
			}
		}
		return out
	}
	function unquote(s,   i, c, q, out) {
		out = ""
		q = ""
		for (i = 1; i <= length(s); i++) {
			c = substr(s, i, 1)
			if (q == "") {
				if (c == "\"" || c == "\047") { q = c; out = out "Q" }
				else out = out c
			} else if (c == "\\" && i < length(s)) {
				i++
			} else if (c == q) {
				q = ""
			}
		}
		return out
	}
	{
		if (match($0, /^[0-9]+:/) == 0) next
		n = substr($0, 1, RLENGTH - 1)
		line = substr($0, RLENGTH + 1)
		# Comments in the languages this tree carries: #, //, --, /* and a
		# block-comment continuation line that starts with *.
		if (line ~ /^[ \t]*(#|\/\/|--|\/\*|\*)/) next
		if (tolower(line) ~ sink && line ~ tokre) { print n ":" line; next }
		# A backtick substitution inside double quotes still executes; a
		# single-quoted one is a literal and stays data.
		if (line ~ btq) { print n ":" line; next }
		if (line ~ executor && line ~ tokre) { print n ":" line; next }
		if (line ~ setupre) { print n ":" line; next }
		if (line ~ qcmd && line !~ arm) { print n ":" line; next }
		if (unquote(line) ~ cmd) { print n ":" line; next }
		# Safety net: the old guard caught a token anywhere, including forms no
		# runner list names. Keep that width for unquoted text, with a word
		# boundary that rejects `no-python` and `python_token`, and with quoted
		# literals blanked so language tags and fences stay data.
		if (blank_quotes(line) ~ word) { print n ":" line; next }
	}
	'
}

while IFS= read -r -d '' file; do
	[ ! -L "$file" ] && [ -f "$file" ] || continue
	case "$file" in
		.github/*|scripts/*|*.sh|*.bash|*.ps1|*.rs|*.lua|*.js|*.ts|*.toml|*.yml|*.yaml|Dockerfile*|*/Dockerfile*|*Makefile*) ;;
		*) [ -x "$file" ] || continue ;;
	esac
	if is_allowed_py "$file"; then continue; fi
	lines=$(grep -anE -- '.' "$file") || {
		status=$?
		if [ "$status" -ne 1 ]; then
			echo "check-no-python: cannot scan repository file: $file" >&2
			exit 1
		fi
		lines=''
	}
	matches=$(printf '%s\n' "$lines" | scan_invocations) || {
		echo "check-no-python: cannot scan repository file: $file" >&2
		exit 1
	}
	while IFS=: read -r line_no line; do
		[ -n "$line_no" ] || continue
		case "$line" in
			'#!'*"$python_token"*) ;;
			*)
				trimmed=$(printf '%s' "$line" | sed 's/^[[:space:]]*//')
				case "$trimmed" in \#*) continue ;; esac
				;;
		esac
		# The whole line must be one allowlisted command, so nothing rides along.
		case "$file" in
			.github/workflows/ci.yml)
				checker='principles|steps|comments|install|butler-path-convention|workflows'
				;;
			.github/workflows/workflow-guard.yml) checker='workflows' ;;
			*) checker='' ;;
		esac
		allowed=0
		if [ -n "$checker" ] && printf '%s\n' "$line" | grep -qxE \
			"[[:space:]]*(- run: |if )?${python_token}3 scripts/check-($checker)\.py( 2>/tmp/out; then| \\\\)?"; then
			allowed=1
		fi
		if [ "$allowed" -ne 1 ]; then
			echo "check-no-python: Python invocation at $file:$line_no: $line" >&2
			failed=1
		fi
	done <<EOF
$matches
EOF
done <"$inventory"

[ "$failed" -eq 0 ] || exit 1
echo "ok — Python files and invocations match the documented temporary allowlist"
