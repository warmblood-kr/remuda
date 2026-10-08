#!/bin/sh
# Plant each #605 bypass in a scratch repo and require the guard to reject it.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
sh "$ROOT/scripts/check-no-python.sh"

fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT HUP INT TERM
py=python3

reset_fixture() {
	rm -rf "$fixture/repo"
	mkdir -p "$fixture/repo/scripts" "$fixture/repo/.github/workflows"
	cp "$ROOT/scripts/check-no-python.sh" "$fixture/repo/scripts/"
	for name in principles steps comments install butler-path-convention workflows; do
		printf '#!/usr/bin/env %s\n' "$py" >"$fixture/repo/scripts/check-$name.py"
	done
	cat >"$fixture/repo/.github/workflows/ci.yml" <<YML
jobs:
  a:
    steps:
      - run: $py scripts/check-principles.py
      - run: |
          if $py scripts/check-steps.py 2>/tmp/out; then
          $py scripts/check-install.py \\
YML
	printf '      - run: %s scripts/check-workflows.py\n' "$py" \
		>"$fixture/repo/.github/workflows/workflow-guard.yml"
	git -C "$fixture/repo" init -q
}

expect_ok() {
	git -C "$fixture/repo" add -A
	if ! sh "$fixture/repo/scripts/check-no-python.sh" >"$fixture/out" 2>&1; then
		cat "$fixture/out" >&2
		echo "guard rejected the clean fixture" >&2
		exit 1
	fi
}

failures=0
expect_rejected() {
	git -C "$fixture/repo" add -A
	if sh "$fixture/repo/scripts/check-no-python.sh" >"$fixture/out" 2>&1; then
		echo "guard accepted: $1" >&2
		failures=$((failures + 1))
	elif ! grep -qF "check-no-python: $2" "$fixture/out"; then
		cat "$fixture/out" >&2
		echo "guard rejected $1 for the wrong reason (wanted: $2)" >&2
		failures=$((failures + 1))
	fi
}

reset_fixture
expect_ok

# 1. Upper-case and .pyw extensions.
reset_fixture; echo 'print(1)' >"$fixture/repo/tool.PY"; expect_rejected 'tool.PY' 'unexpected Python file: tool.PY'
reset_fixture; echo 'print(1)' >"$fixture/repo/tool.pyw"; expect_rejected 'tool.pyw' 'unexpected Python file: tool.pyw'
reset_fixture; echo 'print(1)' >"$fixture/repo/my tool.py"; expect_rejected 'my tool.py' 'unexpected Python file: my tool.py'

# 2. An extensionless Python script outside scripts/ and .github/workflows.
reset_fixture
mkdir -p "$fixture/repo/tests"
printf '#!/usr/bin/env %s\nprint(1)\n' "$py" >"$fixture/repo/tests/helper"
expect_rejected 'extensionless Python script in tests/' 'unexpected Python shebang: tests/helper'

# 3. Invocation forms and packaging the old pattern missed.
for line in "/usr/bin/$py -c 1" "python3.12 -c 1" "pip install x" "uv run x" \
	"uses: actions/setup-python@v5"; do
	reset_fixture
	printf 'steps:\n  - run: %s\n' "$line" >"$fixture/repo/.github/workflows/extra.yml"
	expect_rejected "$line" "Python invocation at .github/workflows/extra.yml:2"
done
reset_fixture; echo 'x' >"$fixture/repo/requirements-dev.txt"; expect_rejected 'requirements-dev.txt' 'unexpected Python requirements file: requirements-dev.txt'

# 4. A second command chained onto an allowlisted line.
reset_fixture
printf '      - run: %s scripts/check-steps.py && %s -c 1\n' "$py" "$py" \
	>>"$fixture/repo/.github/workflows/ci.yml"
expect_rejected 'chained command after an allowlisted invocation' 'Python invocation at .github/workflows/ci.yml:8'

[ "$failures" -eq 0 ] || {
	echo "check-no-python accepted $failures planted bypass(es)" >&2
	exit 1
}
echo 'ok - every planted #605 bypass is rejected'
