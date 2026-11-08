# Working on ultron-rs

ultron-rs is the Rust rewrite of Ultron (github.com/alfredosdpiii/ultron), a fork of Pi (`badlogic/pi-mono`): a
terminal coding agent whose model has one tool, a persistent Python REPL. The `ultron` binary is built from
`crates/cli` and embeds its runtime files from `packages/coding-agent/` (see `crates/cli/build.rs`).

## Style

- Short, direct, technical. No emojis, no filler.
- Answer a question before editing. When responding to feedback, say whether you agree before what you changed.
- Explain a non-trivial design as: problem, a concrete example or short trace, then the fix and why it is needed.

## Done means

A change is done when it works and is verified, not when it is written:

- `cargo fmt --all -- --check` is clean.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` is clean.
- The tests you touched or that cover the change pass; for runtime, host, kernel, prompt or TUI changes, the whole
  `scripts/nextest.sh --workspace --locked`.
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

The codebase was untangled around one question: should these pieces change together? Keep it that way. Before
adding code, decide what it changes with and put it there.

- **No hidden links.** Pieces that must change together live together. No copied logic or values (one owner, one
  helper: `jsrt`, `config::keys`, `config::auth`, `config::paths`, `signals`, `session_journal::catalog`,
  `delegation::usage`). No module reading a global or file another module sets without going through that module's
  API. Keep a feature's code in one crate or folder, not split by layer. No call order or convention callers just
  have to know: make it a type, a single entry point, or a check in `crates/workspace-checks`.
- **No needless links.** Callers depend on behavior, not internals. Stable code never imports more volatile code;
  check the dependency direction when adding a crate dependency. A general function takes no parameter only one
  caller understands. Prefer deep modules: a small interface over a lot of behavior.
- **No surplus.** No pass-through wrappers, no extension points or settings for things that never vary, no dead code.
  Delete what nothing uses.
- **Special cases are design debt.** An `if` for one value, a parameter for one caller, a swallowed error, a retry or
  sleep, a disabled test: each is a requirement the design missed. Fix the design so the case follows from it; keep
  a special case only when the system really needs it, and say why next to it.
- **Explicit state machines.** A lifecycle with more than two states is an enum with one transition function, not
  several bools, Options or status strings set from many places. Settle what a state owes (a reply, a cancel, a
  reservation) before replacing it. Never hold a lock while calling code that can call back into it. State that must
  survive a restart is written before it is acted on, files are written atomically (`config::paths::write_private`:
  temp file, fsync, rename), and recovery handles every persisted state by name. A protocol across threads,
  processes or restarts gets a Quint spec in `specs/` (see `specs/check.sh` and its verify skills) that models both
  the bug class and the fix.
- **Fail closed** on unreadable security state (trust, approvals, credentials).
- **Comments** say what the code cannot: an outside constraint, a reason, a workaround and the problem behind it.
  Don't restate the code.
- **Tests** check behavior and fail on a regression, and only then. A test that repeats the implementation, depends
  on internals, or cannot fail is removed or rewritten. A bug fix comes with a test that fails without the fix.
- A typical change lands in one place. If yours touches many, look for the missing owner first.

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

- Commit only when the user asks, and only files you changed: stage explicit paths, check `git status` first.
- No `Co-Authored-By` or "Generated with" lines in commits or PRs.
- Never: `git reset --hard`, `git checkout .`, `git clean -fd`, `git stash`, `git add -A`, `git add .`,
  `git commit --no-verify`, force pushes. On rebase conflicts in files you didn't change, stop and ask.
- Parallel work goes in a git worktree (`git worktree add -b <branch> ../<dir> main`), with its own
  `CARGO_TARGET_DIR`. After the merge, remove the worktree (`git worktree remove`, `git worktree prune`) and delete
  its target dir.
- To inspect a PR, use `gh pr view`/`gh pr diff` and `git show`; don't switch branches unless asked.
- Post issue or PR comments with `--body-file`, in the user's tone.

If the user's instructions conflict with this file, follow the user after confirming.
