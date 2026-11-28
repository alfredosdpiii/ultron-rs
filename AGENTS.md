# Working on ultron-rs

ultron-rs is the Rust rewrite of Ultron (github.com/alfredosdpiii/ultron), a fork of Pi (`badlogic/pi-mono`): a
terminal coding agent whose model has one tool, a persistent Python REPL. The `ultron` binary is built from
`crates/cli` and embeds its runtime files from `packages/coding-agent/` (see `packages/coding-agent/build.rs`).

## Style

- Short, direct, technical. No emojis, no filler.
- Answer a question before editing. When responding to feedback, say whether you agree before what you changed.
- Explain a non-trivial design as: problem, a concrete example or short trace, then the fix and why it is needed.

## Done means

A change is done when it works and is verified, not when it is written:

- `./scripts/verify.sh` is clean (`cargo fmt`, `ruff`, `loki hygiene`, `clippy -D warnings`, and
  `ultron-workspace-checks`).
- The tests you touched or that cover the change pass; for runtime, host, kernel, prompt or TUI changes, the whole
  `scripts/nextest.sh --workspace --locked`.
- When touching a crate covered by a STITCH++ harness (`stitch/tui`, `stitch/context-control`), run
  `BUILDX_BUILDER=default uvx --from stitch-fuzz==0.2.0 stitch build && stitch check && stitch probe --secs 30`.
- Before a release: the above, plus `scripts/rust-release.sh` and a secret scan of the tree and of the commits since
  the last release.

You may run builds, checks and tests, fix failures and rerun, without asking. Keep going until the change meets the
bar above or something genuinely needs the user; don't end a turn by announcing the next step instead of taking it.

## Tests

- Run tests with `scripts/nextest.sh` (`cargo nextest run` in a systemd scope capped at 6 cores, `ULTRON_TEST_CPUS`,
  at nice 19), which runs every test in a process of its own, 4 at a time (`.config/nextest.toml`). One crate:
  `scripts/nextest.sh -p <crate>`; one file: `scripts/nextest.sh -p <crate> <file>::`. The cap keeps the build and
  the tests from starving the user's desktop; never run them uncapped.
- A crate's `tests/*.rs` files are modules of one test binary, `tests/it.rs`: add a new file's `#[path]` line there.
  A file that re-runs or exits its own test binary (`current_exe`, `process::exit`) gets a `[[test]]` of its own in
  the crate's `Cargo.toml` instead. `crates/workspace-checks` (`every_test_file_is_built`) fails on a file in
  neither.
- Run tests under an empty `HOME`, with no provider keys in the environment. Tests use scripted or fake providers,
  never real ones, and never open a browser, the real clipboard, real Docker or the user's `~/.ultron` or
  `~/.claude`.
- Tests wait on events, never on wall-clock sleeps.
- Tests that need Python use the kernel's interpreter (`ULTRON_PYTHON`, else `/usr/bin/python3`), not bare `python3`.
- Fixtures under `crates/*/fixtures/` that were recorded from the TypeScript product are frozen; don't regenerate
  them. New fixtures are written by Rust tests or by hand.
- Add the GitHub issue number next to a regression test for an issue.

## Live runs

- Any live `ultron` run you start (smoke tests, demos, evals) sets `ULTRON_SERVER_DIR` and `ULTRON_CODING_AGENT_DIR`
  to private temp dirs. Never use the defaults: the user has live sessions.
- No paid model calls unless the user asks for them.

## Secrets

- Never commit credentials, `auth.json`, `.env` files or home-directory paths. Fake keys in tests are built from
  parts at run time where possible, and are obviously fake otherwise.
- Never leave copies of `auth.json` or keys behind; delete temp agent dirs after use.
- Never print a secret in full; show at most its first 6 characters.

## Design

Keep pieces that change together together, and keep boundaries clean:

- **No hidden links.** One owner, one helper (`jsrt`, `config::keys`, `config::auth`, `config::paths`, `signals`,
  `session_journal::catalog`, `delegation::usage`). Keep a feature in one crate/folder; encode call-order invariants
  in types, a single entry point, or `crates/workspace-checks`.
- **No needless links or surplus.** Depend on behavior, not internals; check dependency direction; prefer deep
  modules; delete pass-through wrappers and dead code.
- **Special cases are design debt.** Fix the design instead of piling on one-caller flags, swallowed errors, retries,
  or sleeps; keep a special case only when required and document why.
- **Explicit state machines.** Lifecycles with >2 states are enums with one transition function. Settle what a state
  owes before replacing it; never hold a lock across callbacks; write restart-critical state atomically
  (`config::paths::write_private`) before acting. Protocols across threads/processes/restarts get a TLA+ spec in
  `specs/` (`specs/check.sh`); parsers, renderers, and stateful APIs get STITCH++ harnesses in `stitch/`.
- **Fail closed** on unreadable security state (trust, approvals, credentials).
- **Comments.** Inline `//` comments in `crates/*/src` fail `no_non_exempt_inline_comments_in_src` (use `///`/`//!`
  doc comments for external constraints, reasons, or workarounds; never restate code).
- **Tests** check behavior and fail on regression. Every bug fix ships with a failing-before-fix regression test.
- If a change touches many places, look for the missing owner first.

## Code

- New work goes in `crates/*`. Workspace lints apply: `unsafe_code` is forbidden, clippy warnings are errors.
- `packages/coding-agent/` holds assets the binary embeds; keep their paths, since `build.rs`, `include_str!` and the
  tests resolve them relative to `CARGO_MANIFEST_DIR`.
- Check the crate source in `~/.cargo/registry` for external API types instead of guessing. Fix outdated-dependency
  errors by upgrading, not by removing code.
- Ask before removing functionality that looks intentional. Backward compatibility only when asked.

## Dependencies

- Dependency and `Cargo.lock` changes are reviewed code. Pin direct external deps to exact versions.
- Build and test with `--locked`.

## Git

Several agent sessions may work in this checkout at once.

- Commit only files you changed: stage explicit paths, check `git status` first.
- No `Co-Authored-By` or "Generated with" lines in commits or PRs.
- Never: `git reset --hard`, `git checkout .`, `git clean -fd`, `git stash`, `git add -A`, `git add .`,
  `git commit --no-verify`, force pushes. On rebase conflicts in files you didn't change, stop and ask.
- Parallel work goes in a git worktree (`git worktree add -b <branch> ../<dir> main`), with its own
  `CARGO_TARGET_DIR`. After the merge, remove the worktree (`git worktree remove`, `git worktree prune`) and delete
  its target dir.
- To inspect a PR, use `gh pr view`/`gh pr diff` and `git show`; don't switch branches unless asked.
- Post issue or PR comments with `--body-file`, in the user's tone.

If the user's instructions conflict with this file, follow the user after confirming.
