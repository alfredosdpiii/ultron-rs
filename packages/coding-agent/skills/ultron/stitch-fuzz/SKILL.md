---
name: stitch-fuzz
description: "Coverage-guided stateful API fuzzing with STITCH++ (stitch-fuzz) in Rust, Python, TypeScript/JS, Go, C/C++, Java, Ruby, or Swift. Chains typed stubs into programs, mutates both call sequences and arguments, checks invariants, and minimizes crashes into frozen Docker reproducers + git bisect. Use for /skill:stitch-fuzz, fuzzing a library or crate, finding panics/crashes/stateful bugs, or writing a STITCH++ harness."
metadata:
  source: STITCH++ agent guide (https://stitch.c.mov/agents/overview.raw.md)
---

# STITCH++ Stateful API Fuzzing

STITCH++ fuzzes a library through its real API by chaining small typed **stubs** into stateful **programs**,
passing returned objects from one call to the next, mutating both arguments and call graphs under coverage
guidance, and reporting crashes, sanitizer findings, undeclared exceptions, timeouts, and failed `check(...)`
invariants. Everything builds and runs in Docker (`docker` + `buildx` required).

## Running & Version Pinning

Run without global install via `uvx` (or `pip install stitch-fuzz`):

```sh
uvx --from stitch-fuzz stitch --version
uvx --from stitch-fuzz stitch guide            # overview
uvx --from stitch-fuzz stitch guide rust       # language guide (rust, python, javascript, go, cpp, ...)
uvx --from stitch-fuzz stitch guide stubs      # stub rules & data types
```

Every `stitch/` project pins `stitch_version` in `stitch.toml`. Always run the exact pinned version
(`uvx --from stitch-fuzz==<ver> stitch ...`); never edit the pin to silence a mismatch error.

## Core Concepts

- **Object**: a handle produced by one stub (`-> New<T>`) and passed to later stubs (`Borrow<T>` keeps it live;
  `Take<T>` consumes it).
- **Fuzzed parameter**: every non-object argument (`Str<N, DataType>`, `Range<A, B>`, `Bytes<N>`, enums, numbers).
- **Bail**: an expected failure (`Err(...)` / invalid input / unmet precondition) — not a bug; dependent calls skip.
- **Bug**: a crash, panic, sanitizer report, undeclared exception, timeout, or `check(cond, "why")` returning false.
- **Data type / decoder**: tags on string/byte params that attach seed corpora, dictionaries, or structured decoders.

## 9-Step Workflow

1. **Create project**: `stitch init <dir> --lang <lang> --path <checkout>` (or `--git <url> --rev <tag>`).
2. **Build image**: edit `Dockerfile` until `stitch build --image-only` succeeds. For Cargo workspaces, copy the
   workspace to `/src` and point `[rust.dependencies]` at `/src/crates/<crate>`.
3. **Write stubs**: read the library's tests *before* docs to capture the true API contract. Write producers first,
   then mutators/operations, then `check(...)` roundtrip/invariant stubs.
4. **Build & check**: `stitch build && stitch check` — proceed only when clean with zero warnings.
5. **Probe**: `stitch probe --secs 60` — fix any stub under "never succeeds" or "often skipped", and keep bail rate
   below 50% (see `stitch guide probe`).
6. **Fuzz**: `stitch fuzz --jobs 4` (resumes corpus; inspect with `stitch status`). For long runs in Ultron's REPL,
   launch via `bash(..., yield_after=0)`.
7. **Triage**: `stitch crashes`, classify every crash as harness bug vs real library bug (`stitch guide triage`).
8. **Coverage**: `stitch cov` — add stubs for uncovered functions, then rebuild and probe again.
9. **Report**: for each confirmed library bug, generate a frozen reproducer (`stitch report <id>`) and bisect if
   applicable (`stitch bisect <id>`), reporting location, upstream status, reproducer path, and `stitch_version`.

## Rules

- Read `stitch guide <lang>` and `stitch guide stubs` before writing stubs.
- Never add guards the library itself doesn't require — extra guards hide real bugs from the fuzzer.
- Do not report a library bug until triaged and reproduced cleanly via `stitch repro <id>`.
- Pass `--json` to any `stitch` command whose output you parse programmatically.
- If `docker buildx` defaults to a `docker-container` builder and fails to resolve local `stitch/*` base images, prefix commands with `BUILDX_BUILDER=default`.
- Ensure the target repo has a `.dockerignore` excluding `target/`, `.git/`, and `node_modules/` when using `[source] path = ...` so Docker doesn't transfer gigabytes of build artifacts.
