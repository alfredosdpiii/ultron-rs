#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGETS=(
  "$HOME/.local/bin/ultron"
  "$HOME/.ultron/rust/bin/ultron"
)
SERVICES=(
  "ultron-durable-1023f3eead80.service"
  "ultron-autoreview.service"
)

echo "==> Building release binary (CPUQuota=600%, nice 19)"
systemd-run --user --scope --quiet --collect -p CPUQuota=600% -- \
  nice -n 19 cargo build --manifest-path "$ROOT/Cargo.toml" --release --locked -p ultron-agent --bin ultron

SRC="$ROOT/target/release/ultron"
for dst in "${TARGETS[@]}"; do
  mkdir -p "$(dirname "$dst")"
  tmp="$(dirname "$dst")/.ultron.install.$$.tmp"
  cp "$SRC" "$tmp"
  chmod 0755 "$tmp"
  mv -f "$tmp" "$dst"
done

echo "==> Verifying SHA-256 parity"
sha256sum "$SRC" "${TARGETS[@]}"

echo "==> Restarting user services"
systemctl --user restart "${SERVICES[@]}"
systemctl --user is-active "${SERVICES[@]}"
