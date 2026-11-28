### Fuzzing

**You are the owner and long-term maintainer of this codebase, and its design is up to you.** Use STITCH++ (`stitch-fuzz`, see `/skill:stitch-fuzz`) to test real compiled code across adversarial stateful API call sequences — complementing TLA+ (which checks abstract models) and unit tests (which check hand-written inputs).

1. Check if the touched crate/library already has a `stitch/<target>/` harness (for example `stitch/tui` or `stitch/context-control`). If not, initialize one in a dedicated folder (`stitch init <dir> --lang <lang> --path <repo>`) and ensure the repo has a `.dockerignore` excluding `target/`, `.git/`, and `node_modules/`.
2. Write or extend typed stubs (`endpoint! { ... }`) for the target APIs and add `check(cond, "...")` invariant assertions. Read the library's tests before its docs; never add guards the library itself does not require.
3. Build and validate cleanly (use `BUILDX_BUILDER=default` if Docker's active buildx builder uses the `docker-container` driver):
   ```sh
   BUILDX_BUILDER=default uvx --from stitch-fuzz==0.2.0 stitch build
   BUILDX_BUILDER=default uvx --from stitch-fuzz==0.2.0 stitch check
   ```
4. Run a short probe first (`stitch probe --secs 30`) and fix any stub with high bail rate (>50%) or skipped calls.
5. For deeper hardening, launch a multi-worker campaign via `bash(..., yield_after=0)` (`stitch fuzz --jobs 4`), inspect with `stitch status` and `stitch crashes`, and stop cleanly when done (`stitch stop <name>`).
6. For every reproducible crash, inspect `stitch exec <id> --trace`, classify harness bug vs real library bug, add a permanent regression test with the minimized input, fix the root cause, rebuild `stitch`, and verify `stitch exec <id>` reports `no crash`.
7. Run **Opening a PR**.

**Reply:** targets/endpoints fuzzed, probe/campaign stats (execs, edges, bail %), every crash found and fixed (with `stitch exec` before/after), and regression tests added.
