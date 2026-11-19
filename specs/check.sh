#!/usr/bin/env bash
# Checks a TLA+ spec under specs/ exhaustively and compares each rule's verdict with the expected one.
#
#   specs/check.sh <spec dir> [config ...]     e.g. specs/check.sh specs/spawn-collect
#
# <spec dir> holds one <name>.tla, its TLC configs (*.cfg: constants, INVARIANT, CONSTRAINT) and `expected`, one line
# per config: `<config> <rule>=holds|breaks ...`. Every rule is checked in a run of its own, so a rule that breaks
# does not hide the others. Exit 0 when every verdict is the expected one, 1 when one differs, 2 when a tool fails.
#
# The checker is everystate 0.3.1 (github.com/mattzcarey/everystate): TLC's command line, output and verdicts as one
# native binary, no Java. EVERYSTATE=path, else everystate on PATH, else npx.
set -euo pipefail

EVERYSTATE_VERSION=0.3.1

dir=${1:?usage: specs/check.sh <spec dir> [config ...]}
shift
dir=$(cd "$dir" && pwd)
specs=("$dir"/*.tla)
[ ${#specs[@]} -eq 1 ] && [ -f "${specs[0]}" ] || { echo "$dir must hold one .tla file" >&2; exit 2; }
module=$(basename "${specs[0]}" .tla)
[ -f "$dir/expected" ] || { echo "$dir has no expected file" >&2; exit 2; }

work=$(mktemp -d "${TMPDIR:-/tmp}/ultron-specs.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir "$work/tla"
cp "${specs[0]}" "$work/tla/"

if [ -n "${EVERYSTATE:-}" ]; then checker=("$EVERYSTATE")
elif command -v everystate > /dev/null; then checker=(everystate)
else checker=(npx --yes "everystate@$EVERYSTATE_VERSION")
fi

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
    (cd "$work" && "${checker[@]}" -workers 1 -deadlock \
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
