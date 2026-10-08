#!/bin/sh
# Plant each #605 bypass in a scratch repo and require the guard to reject it.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bash "$ROOT/scripts/check-no-python.sh"

fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT HUP INT TERM
py=py''thon3
req_file=require''ments-dev.txt

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
	if ! bash "$fixture/repo/scripts/check-no-python.sh" >"$fixture/out" 2>&1; then
		cat "$fixture/out" >&2
		echo "guard rejected the clean fixture" >&2
		exit 1
	fi
}

failures=0
expect_rejected() {
	if [ "${3:-stage}" != no-stage ]; then git -C "$fixture/repo" add -A; fi
	if bash "$fixture/repo/scripts/check-no-python.sh" >"$fixture/out" 2>&1; then
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
for line in "/usr/bin/$py -c 1" "$py.12 -c 1" "pi""p install x" "u""v run x" \
	"uses: actions/setup-py""thon@v5"; do
	reset_fixture
	printf 'steps:\n  - run: %s\n' "$line" >"$fixture/repo/.github/workflows/extra.yml"
	expect_rejected "$line" "Python invocation at .github/workflows/extra.yml:2"
done
reset_fixture; echo 'x' >"$fixture/repo/$req_file"; expect_rejected "$req_file" "unexpected Python requirements file: $req_file"

# 4. A second command chained onto an allowlisted line.
reset_fixture
printf '      - run: %s scripts/check-steps.py && %s -c 1\n' "$py" "$py" \
	>>"$fixture/repo/.github/workflows/ci.yml"
expect_rejected 'chained command after an allowlisted invocation' 'Python invocation at .github/workflows/ci.yml:8'

# Preserve complete Git filenames in both scans.
for file in 'scripts/my tool.sh' 'scripts/도구.sh'; do
	reset_fixture
	printf '%s -c 1\n' "$py" >"$fixture/repo/$file"
	expect_rejected "$file" "Python invocation at $file:1"
done
for file in "$(printf 'tool\t.py')" "$(printf 'tool\n.py')" 'tool".py'; do
	reset_fixture
	printf 'print(1)\n' >"$fixture/repo/$file"
	expect_rejected 'quoted Git filename' 'unexpected Python file:'
done

# Cover command quoting and source outside scripts/workflows.
for line in "\"$py\" -c 1" "$(printf 'echo `%s -c 1`' "$py")" "pi""p.exe install x"; do
	reset_fixture
	printf '%s\n' "$line" >"$fixture/repo/.github/workflows/extra.yml"
	expect_rejected 'quoted command' 'Python invocation at .github/workflows/extra.yml:1'
done
for file in Makefile Dockerfile docs/install.sh native/src/probe.rs .github/actions/probe/action.yml; do
	reset_fixture
	mkdir -p "$(dirname "$fixture/repo/$file")"
	printf '%s -c 1\n' "$py" >"$fixture/repo/$file"
	expect_rejected "$file" "Python invocation at $file:1"
done

# Neither checker is exempt from its own policy. The function is never called.
reset_fixture
printf '\nunused_probe() { %s -c 1; }\n' "$py" >>"$fixture/repo/scripts/check-no-python.sh"
expect_rejected 'guard source' 'Python invocation at scripts/check-no-python.sh:'
reset_fixture
printf '#!/bin/sh\n%s -c 1\n' "$py" >"$fixture/repo/scripts/test-no-python.sh"
expect_rejected 'test source' 'Python invocation at scripts/test-no-python.sh:2'

# Missing files, symlinks, and submodule directories must not disappear.
reset_fixture
ln -s missing "$fixture/repo/tool.py"
expect_rejected 'dangling symlink' 'cannot inspect regular repository file: tool.py'
reset_fixture
git -C "$fixture/repo" update-index --add --cacheinfo 160000,1111111111111111111111111111111111111111,embedded
expect_rejected 'unpopulated submodule' 'cannot inspect regular repository file: embedded' no-stage
reset_fixture
printf 'print(1)\n' >"$fixture/repo/tool.py"
git -C "$fixture/repo" add tool.py
rm "$fixture/repo/tool.py"
expect_rejected 'missing indexed file' 'cannot inspect regular repository file: tool.py' no-stage

# Tool failures are not empty, successful scans.
reset_fixture
mkdir -p "$fixture/bin"
cat >"$fixture/bin/git" <<'SH'
#!/bin/sh
case "$1" in ls-files|-c) exit 71 ;; esac
exec /usr/bin/git "$@"
SH
chmod +x "$fixture/bin/git"
PATH="$fixture/bin:$PATH" expect_rejected 'Git enumeration error' 'cannot enumerate repository files'
rm -f "$fixture/bin/git"
reset_fixture
cat >"$fixture/bin/grep" <<'SH'
#!/bin/sh
case "$1" in -nE|-anE) exit 2 ;; esac
exec /usr/bin/grep "$@"
SH
chmod +x "$fixture/bin/grep"
PATH="$fixture/bin:$PATH" expect_rejected 'content scan error' 'cannot scan repository file:'
rm -f "$fixture/bin/grep"
reset_fixture
printf '#!/bin/sh\nexit 2\n' >"$fixture/bin/head"
chmod +x "$fixture/bin/head"
PATH="$fixture/bin:$PATH" expect_rejected 'first-line read error' 'cannot read repository file:'
rm -f "$fixture/bin/head"

# Policy predicates must distinguish no match from a tool failure too.
for probe in extension packaging shebang; do
	reset_fixture
	selector=-qiE
	case "$probe" in
		extension) printf '\n' >"$fixture/repo/tool.py" ;;
		packaging) printf '\n' >"$fixture/repo/$req_file" ;;
		shebang)
			mkdir -p "$fixture/repo/tests"
			printf '#!/usr/bin/env %s\n' "$py" >"$fixture/repo/tests/helper"
			selector=-qE
			;;
	esac
	cat >"$fixture/bin/grep" <<SH
#!/bin/sh
case "\$1" in $selector) exit 2 ;; esac
exec /usr/bin/grep "\$@"
SH
	chmod +x "$fixture/bin/grep"
	PATH="$fixture/bin:$PATH" expect_rejected "$probe predicate error" 'cannot match repository policy for:'
	rm -f "$fixture/bin/grep"
done

[ "$failures" -eq 0 ] || {
	echo "check-no-python accepted $failures planted bypass(es)" >&2
	exit 1
}
echo 'ok - every planted #605 bypass is rejected'
