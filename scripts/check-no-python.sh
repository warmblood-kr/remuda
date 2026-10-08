#!/bin/sh
# Fail when Python enters the core tree outside the temporary, owned port list.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

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

failed=0
while IFS= read -r file; do
	[ -f "$file" ] || continue
	case "$file" in
		scripts/check-no-python.sh|scripts/test-no-python.sh) continue ;;
	esac
	if is_allowed_py "$file"; then continue; fi
	# Extensions in any case, packaging files, and Python shebangs anywhere.
	if printf '%s\n' "$file" | grep -qiE '\.pyw?$'; then
		echo "check-no-python: unexpected Python file: $file" >&2
		failed=1
	elif printf '%s\n' "${file##*/}" | grep -qiE '^requirements.*\.(txt|in)$'; then
		echo "check-no-python: unexpected Python requirements file: $file" >&2
		failed=1
	elif head -n 1 "$file" 2>/dev/null | grep -qE '^#!.*python'; then
		echo "check-no-python: unexpected Python shebang: $file" >&2
		failed=1
	fi
done <<EOF
$(git -c core.quotePath=false ls-files --cached --others --exclude-standard)
EOF

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

# Check tracked and untracked source in the two directories where build scripts
# and workflow commands live. Comments are ignored; Python shebangs are not.
source_files=$(git ls-files --cached --others --exclude-standard -- .github/workflows scripts)
for file in $source_files; do
	[ -f "$file" ] || continue
	case "$file" in
		scripts/check-no-python.sh|scripts/test-no-python.sh) continue ;;
		scripts/*.py)
			if is_allowed_py "$file"; then continue; fi
			;;
	esac
	while IFS=: read -r line_no line; do
		[ -n "$line_no" ] || continue
		case "$line" in
			'#!'*python*) ;;
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
			"[[:space:]]*(- run: |if )?python3 scripts/check-($checker)\.py( 2>/tmp/out; then| \\\\)?"; then
			allowed=1
		fi
		if [ "$allowed" -ne 1 ]; then
			echo "check-no-python: Python invocation at $file:$line_no: $line" >&2
			failed=1
		fi
	done <<EOF
$(grep -nE '(^|[/[:space:]|;&(])(python[0-9.]*|pip[0-9.]*|uv)([[:space:]]|$)|setup-python|requirements[^[:space:]]*\.(txt|in)|^#!.*python' "$file" || true)
EOF
done

[ "$failed" -eq 0 ] || exit 1
echo "ok — Python files and invocations match the documented temporary allowlist"
