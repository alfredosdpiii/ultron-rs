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

echo "==> Ensuring latest loki-guardrails CLI is installed"
LATEST_LOKI="$(cargo info loki-guardrails 2>/dev/null | awk '/^version:/ {print $2; exit}' || true)"
INSTALLED_LOKI="$("$HOME/.local/bin/loki" --version 2>/dev/null | awk '{print $2}' || true)"
if [[ -n "$LATEST_LOKI" && "$INSTALLED_LOKI" != "$LATEST_LOKI" ]]; then
  echo "    Installing loki-guardrails $LATEST_LOKI (was: ${INSTALLED_LOKI:-none})"
  cargo install loki-guardrails --locked --force --root "$HOME/.local"
else
  echo "    loki ${INSTALLED_LOKI:-unknown} is up to date (latest: ${LATEST_LOKI:-offline})"
fi
PINNED_LOKI="$(awk -F'"' '/loki-guardrails/ {for (i=1;i<=NF;i++) if ($i ~ /^=/) {sub(/^=/,"",$i); print $i; exit}}' "$ROOT/Cargo.toml")"
if [[ -n "$LATEST_LOKI" && -n "$PINNED_LOKI" && "$PINNED_LOKI" != "$LATEST_LOKI" ]]; then
  echo "    Note: Cargo.toml pins loki-guardrails =$PINNED_LOKI (crates.io latest is $LATEST_LOKI)"
fi

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
