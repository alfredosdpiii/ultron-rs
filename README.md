# ultron-rs

The Rust rewrite of [Ultron](https://github.com/alfredosdpiii/ultron): a terminal coding agent whose model has one
tool, a persistent Python REPL. Ultron is a fork of [Pi](https://github.com/badlogic/pi-mono); this repository
holds only the Rust product, with its history filtered out of the Ultron repository.

The model's tool list is one entry, `rlm`. Shell commands, edits, file reads, MCP servers, sub-agents and sub-model
calls are async Python functions inside the REPL. The model writes a cell that loops, filters and fans out, keeps
results in variables, and only what it prints comes back into its context.

The `ultron` binary is self-contained: no Node.js. It embeds its runtime files (the Python REPL runtime, bundled
skills, docs and changelog; see `crates/cli/build.rs`) and needs only Python 3 at run time: `ULTRON_PYTHON` if set,
else `/usr/bin/python3` on Linux, else `python3` on `PATH`.

## Build and run

```bash
cargo build --release -p cli         # target/release/ultron
target/release/ultron setup          # pick a provider and model
target/release/ultron --help
```

The first run that needs them installs the runtime files into `~/.ultron/agent/runtime/<version>-<hash>/` (under
`$ULTRON_CODING_AGENT_DIR` when set).

## Layout

- `crates/`: the Cargo workspace. `cli` is the `ultron` binary; `kernel` supervises the Python REPL; `host`,
  `kernel-apis`, `inference` and `delegation` implement the operations behind it; `session-core`, `daemon`,
  `server`, `client` and `protocol` run sessions; `ai` is the provider layer; `tui` and `interactive` the terminal
  UI; `loki` the write-time guardrails; `review` the `/review` and `ultron autoreview` engine.
- `packages/coding-agent/`: the assets the binary embeds or the tests read, kept at their original paths: the Python
  runtime (`src/ultron/rlm/*.py`, `secret-patterns.json`), the Loki engine, the bundled `skills/`, `docs/`,
  `CHANGELOG.md` and `package.json` (the version), the `--export` HTML template and the built-in themes.
- `scripts/rust-release.sh`, `scripts/rust-release/`: the release archive (binary, `install.sh`, licenses).
- `docs/rust-rewrite/`: the rewrite's baseline, decision log and feature ledger.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
scripts/rust-release.sh              # dist/rust-release/ultron-<version>-<target>.tar.gz; publishes nothing
```

See [AGENTS.md](AGENTS.md) for the rules agents and contributors follow.

## License

MIT, as Pi: see [LICENSE](LICENSE). Third-party notices for what the binary embeds are in
[scripts/rust-release/NOTICE](scripts/rust-release/NOTICE); the release archive adds the Rust crates' licenses.
Bundled Loki engine and skills carry their own MIT licenses beside them
(`packages/coding-agent/src/ultron/loki-engine/LICENSE`, `packages/coding-agent/skills/*/LICENSE`).
