---
name: verify-ultron
description: Deterministic verification, release build, dual-binary install, and systemd service restart checklist for ultron-rs.
---

# Verify & Ship `ultron-rs`

Run this skill before reporting any change to `ultron-rs` done.

## 1. Static & Test Verification
Run the unified verifier (passing `-p <crate>` for every crate you touched):

```bash
./scripts/verify.sh -p ultron-kernel -p ultron-delegation -p ultron-session-core
```

`scripts/verify.sh` enforces:
1. `cargo fmt --all -- --check`
2. `python3 scripts/strip-comments.py --check crates/*/src` (no non-exempt inline comments)
3. `loki --root . hygiene --base HEAD`
4. `cargo clippy --workspace --all-targets --locked -- -D warnings`
5. CPU-capped `./scripts/nextest.sh -p ultron-workspace-checks ...` (enforces `no_unwrap_in_prod_src`, `no_lock_expect_in_prod_src`, `no_non_exempt_inline_comments_in_src`, `no_new_god_files`, `every_workspace_crate_inherits_workspace_lints`, `every_loki_guardrails_pin_is_exact_and_consistent`, `every_test_environment_turns_memory_services_off`, `every_test_file_is_built`)

## 2. Build, Install Both Binaries, & Restart Services
Never leave `/home/bryan/.local/bin/ultron` and `/home/bryan/.ultron/rust/bin/ultron` out of sync with `HEAD`:

```bash
./scripts/install-local.sh
```

Verify both SHA-256 digests match `target/release/ultron` and both user services report `active`.

## 3. Git Ship
- Stage explicit paths only (never `git add -A`).
- Never add `Co-Authored-By` trailers.
- Commit and push `origin main` before final install (or re-run `install-local.sh` after any follow-up commit).
