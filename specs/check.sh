#!/usr/bin/env bash
# Checks a Quint spec under specs/ exhaustively with TLC and compares each rule's verdict with the expected one.
#
#   specs/check.sh <spec dir> [config ...]     e.g. specs/check.sh specs/spawn-collect
#
# <spec dir> holds one <name>.qnt, its TLC configs (*.cfg: constants, INVARIANT, CONSTRAINT) and `expected`, one line
# per config: `<config> <rule>=holds|breaks ...`. Every rule is checked in a TLC run of its own, so a rule that breaks
# does not hide the others. Exit 0 when every verdict is the expected one, 1 when one differs, 2 when a tool fails.
#
# Tools, pinned: Quint 0.33.1 (QUINT=path, else npx), which compiles to TLA+ through Apalache 0.62.4 (it downloads it
# to $QUINT_HOME, default ~/.quint, and needs Java); TLC 1.7.4 (TLA2TOOLS=path to tla2tools.jar, else downloaded once
# to ~/.cache/ultron-specs and checked against its SHA-256); Java 11 or later (JAVA=path, else java on PATH).
set -euo pipefail

QUINT_VERSION=0.33.1
APALACHE_VERSION=0.62.4
TLC_VERSION=1.7.4
TLC_SHA256=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88

dir=${1:?usage: specs/check.sh <spec dir> [config ...]}
shift
dir=$(cd "$dir" && pwd)
specs=("$dir"/*.qnt)
[ ${#specs[@]} -eq 1 ] || { echo "$dir must hold one .qnt file" >&2; exit 2; }
spec=${specs[0]}
[ -f "$dir/expected" ] || { echo "$dir has no expected file" >&2; exit 2; }

java=${JAVA:-java}
work=$(mktemp -d "${TMPDIR:-/tmp}/ultron-specs.XXXXXX")
trap 'rm -rf "$work"' EXIT

jar=${TLA2TOOLS:-}
if [ -z "$jar" ]; then
  cache=${XDG_CACHE_HOME:-$HOME/.cache}/ultron-specs
  jar=$cache/tla2tools-$TLC_VERSION.jar
  if [ ! -f "$jar" ]; then
    mkdir -p "$cache"
    curl -fsSL -o "$work/tla2tools.jar" \
      "https://github.com/tlaplus/tlaplus/releases/download/v$TLC_VERSION/tla2tools.jar"
    echo "$TLC_SHA256  $work/tla2tools.jar" | sha256sum -c --quiet - || { echo "tla2tools.jar checksum differs" >&2; exit 2; }
    mv "$work/tla2tools.jar" "$jar"
  fi
fi

quint() {
  if [ -n "${QUINT:-}" ]; then "$QUINT" "$@"; else npx --yes "@informalsystems/quint@$QUINT_VERSION" "$@"; fi
}

# Quint's TLA+ starts at its MODULE line (Apalache's server log shares stdout) and extends Apalache and Variants,
# which are read from the Apalache jar Quint downloaded.
mkdir "$work/tla"
java_dir=$(dirname "$(command -v "$java")")
if ! (cd "$work" && PATH="$java_dir:$PATH" quint compile --target tlaplus --apalache-version "$APALACHE_VERSION" \
  "$spec" > compiled.out 2> compiled.err); then
  cat "$work/compiled.err" "$work/compiled.out" >&2
  exit 2
fi
sed -n '/^-\{4,\} *MODULE /,$p' "$work/compiled.out" > "$work/tla/module.tla"
module=$(sed -n '1s/^-* *MODULE \([A-Za-z0-9_]*\) .*/\1/p' "$work/tla/module.tla")
[ -n "$module" ] || { echo "Quint wrote no TLA+ module" >&2; cat "$work/compiled.err" >&2; exit 2; }
mv "$work/tla/module.tla" "$work/tla/$module.tla"
apalache_jar=${QUINT_HOME:-$HOME/.quint}/apalache-dist-$APALACHE_VERSION/apalache/lib/apalache.jar
for m in Apalache Variants; do
  python3 -I -c 'import sys, zipfile; sys.stdout.buffer.write(zipfile.ZipFile(sys.argv[1]).read(sys.argv[2]))' \
    "$apalache_jar" "tla2sany/StandardModules/$m.tla" > "$work/tla/$m.tla"
done

configs=("$@")
[ ${#configs[@]} -gt 0 ] || mapfile -t configs < <(awk '{print $1}' "$dir/expected")
status=0
for config in "${configs[@]}"; do
  line=$(awk -v c="$config" '$1 == c' "$dir/expected")
  [ -n "$line" ] || { echo "$config: no line in expected" >&2; exit 2; }
  for pair in ${line#* }; do
    rule=${pair%%=*}
    want=${pair#*=}
    # TLC reads the config beside the module.
    cfg=$work/tla/$config-$rule.cfg
    grep -v '^INVARIANT' "$dir/$config.cfg" > "$cfg"
    echo "INVARIANT $rule" >> "$cfg"
    out=$work/$config-$rule.out
    set +e
    (cd "$work" && "$java" -XX:+UseParallelGC -cp "$jar" tlc2.TLC -workers 1 -deadlock \
      -metadir "$work/meta-$config-$rule" -config "$cfg" "$work/tla/$module.tla" > "$out" 2>&1)
    code=$?
    set -e
    if [ $code -eq 0 ]; then got=holds
    elif grep -q "Invariant $rule is violated" "$out"; then got=breaks
    else echo "$config $rule: TLC exited $code" >&2; tail -20 "$out" >&2; exit 2
    fi
    states=$(sed -n 's/.* \([0-9]*\) distinct states found.*/\1/p' "$out" | tail -1)
    if [ "$got" = "$want" ]; then
      echo "ok    $config $rule $got ($states states)"
    else
      echo "DIFF  $config $rule $got, expected $want ($states states)"
      sed -n '/^Error: The behavior up to this point is:/,/^[0-9]* states generated/p' "$out"
      status=1
    fi
  done
done
exit $status
