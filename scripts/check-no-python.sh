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
	if printf '%s\n' "$file" | grep -qiE '\.pyw?$'; then
		echo "check-no-python: unexpected Python file: $file" >&2
		failed=1
	elif printf '%s\n' "${file##*/}" | grep -qiE '^requirements.*\.(txt|in)$'; then
		echo "check-no-python: unexpected Python requirements file: $file" >&2
		failed=1
	elif grep -qE '^#!.*py''thon' <<<"$first_line"; then
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
invocation_re="(^|[^[:alnum:]_-])(${python_token}[0-9.]*|${pip_token}[0-9.]*|${uv_token})([^[:alnum:]_-]|$)|${setup_token}|requirements[^[:space:]]*\\.(txt|in)|^#!.*${python_token}"
while IFS= read -r -d '' file; do
	[ ! -L "$file" ] && [ -f "$file" ] || continue
	case "$file" in
		.github/*|scripts/*|*.sh|*.bash|*.ps1|*.rs|*.lua|*.js|*.ts|*.toml|*.yml|*.yaml|Dockerfile*|*/Dockerfile*|*Makefile*) ;;
		*) [ -x "$file" ] || continue ;;
	esac
	if is_allowed_py "$file"; then continue; fi
	matches=$(grep -anE -- "$invocation_re" "$file") || {
		status=$?
		if [ "$status" -ne 1 ]; then
			echo "check-no-python: cannot scan repository file: $file" >&2
			exit 1
		fi
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
