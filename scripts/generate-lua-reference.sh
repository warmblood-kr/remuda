#!/bin/sh
set -eu

output=${1:-docs/lua-reference.rst}
html_output=${2:-docs/lua-reference.html}
runtime_dir=$(mktemp -d "${TMPDIR:-/tmp}/remuda-doc.XXXXXX")
trap 'rm -rf "$runtime_dir"' EXIT HUP INT TERM

command -v pandoc >/dev/null 2>&1 || {
  echo "pandoc is required to render the generated RST reference as HTML" >&2
  exit 1
}

cargo build -p remuda-native --bin remuda
REMUDA_RUNTIME_DIR="$runtime_dir" REMUDA_NO_UPDATE_CHECK=1 \
  target/debug/remuda doc >"$output"

tmp_output="$runtime_dir/reference.rst"
awk '{ lines[NR] = $0 } END {
  while (NR > 0 && lines[NR] == "") NR--
  for (line = 1; line <= NR; line++) print lines[line]
}' "$output" >"$tmp_output"
mv "$tmp_output" "$output"

grep -q '^Remuda Lua runtime$' "$output"
pandoc --from=rst --to=html5 --wrap=none "$output" >"$html_output"
grep -q '<h1' "$html_output"
echo "generated $output"
echo "generated $html_output"
