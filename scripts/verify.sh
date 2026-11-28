#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "==> ruff check packages/coding-agent/src/ultron/rlm"
ruff check packages/coding-agent/src/ultron/rlm

echo "==> loki hygiene --base HEAD"
loki --root . hygiene --base HEAD

echo "==> cargo clippy --workspace --all-targets --locked -- -D warnings"
nice -n 19 cargo clippy --workspace --all-targets --locked -- -D warnings

echo "==> scripts/nextest.sh (workspace-checks + caller args)"
if [[ $# -eq 0 ]]; then
  ./scripts/nextest.sh -p ultron-workspace-checks 2>&1 | cat
else
  ./scripts/nextest.sh -p ultron-workspace-checks "$@" 2>&1 | cat
fi
