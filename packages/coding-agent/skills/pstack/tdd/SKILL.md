---
name: tdd
description: "Use only when the user explicitly asks for TDD, a failing test, or a regression test, OR when the bug has an obvious cheap local test target. Skip when the test path is unclear, expensive, integration-heavy, or not requested."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/tdd
  author: Lauren Tan
  modified: ported to Ultron's REPL; steps enforced as a procedure
procedure:
  inputs:
    test_command: the one command that runs only the new regression test
    test_files: globs of the test files the regression test may add or change
    fix_files: globs of the production files the fix may change
  segments:
    - name: failing-test
      goal: >-
        Write the smallest focused test that would have caught the bug. It encodes the intended behavior, not the
        current implementation. Change test files only; production code stays as it is.
      writes: ["{{test_files}}"]
      check: {run: "{{test_command}}", expect: fail}
      max_iters: 3
    - name: right-reason
      goal: >-
        State the bug (intended versus current behavior) and why the failing output shows exactly that. Change no
        files.
      writes: []
      judge:
        kind: skill.tdd
        questions:
          right_reason:
            type: yesno
            instructions: >-
              The evidence is the regression test's failing output; the statement says what the bug is. Answer true
              only if the output shows the test failing on the described behavior (an assertion about it), not on a
              typo, a compile or import error, a missing fixture or an unrelated failure.
        pass: {right_reason: true}
      max_iters: 2
    - name: fix
      goal: >-
        Make the smallest production change that gives the intended behavior while keeping nearby contracts. Do not
        touch the test.
      writes: ["{{fix_files}}"]
      check: "{{test_command}}"
      max_iters: 5
---

# TDD Bug Fix

When fixing a bug with a clear, cheap test path, make the broken behavior executable before changing production code. The goal is a focused regression test that fails before the fix and passes after it.

Do not force a test when it would be impractical. If the available test would require broad harness setup, brittle mocks, slow end-to-end infrastructure, production-only state, vague reproduction steps, or large unrelated fixture churn, skip adding a new test and use the closest useful verification instead.

## Workflow

Run it as a procedure: the host checks each step itself, so a test that never failed or a fix that touched the
test cannot pass.

```python
run = await skills.run("tdd", input={
    "test_command": "npx vitest --run test/x.test.ts -t 'parses empty input'",
    "test_files": ["test/x.test.ts"],
    "fix_files": ["src/parser/**"],
})
# run.segment says what to do now; do it in your own cells, then:
await run.check()                         # failing-test: the host runs test_command and it must fail
await run.check(state="Empty input should parse to []; the assertion fails with undefined.")  # right-reason
await run.check()                         # fix: the test must pass; the host runs it once more at the end
```

Each `run.check()` returns the run's status (`help(SkillRun)`): a failed step leaves you on the same segment
with `tries_left`; `outcome` "passed" ends it. Choose `test_command` before you start (step 2): it is fixed for the
run. The steps it walks:

1. **Understand the bug.** Identify the intended behavior, current behavior, affected path, and smallest observable reproduction.
2. **Choose the narrowest executable check.** Prefer the closest unit, component, integration, or regression test already used for that codepath. If no practical test path is obvious, do not create one from scratch just to satisfy the workflow.
3. **Write the failing test first.** Add the smallest focused test that would have caught the bug. The test should encode intended behavior, not mirror the current implementation.
4. **Run the new test before fixing.** Run only that test (the `failing-test` check does). Confirm it fails for the intended reason (the `right-reason` judgment). If it passes or fails for an unrelated reason, correct the test or reproduction before editing the implementation.
5. **Fix the bug.** Make the smallest production change that satisfies the intended behavior while preserving nearby contracts.
6. **Rerun the regression test.** The `fix` check runs the same command and it must pass.

Without a cheap test command, skip the procedure and follow the steps by hand.

## If a Failing Test Is Impractical

Use the closest executable regression check instead: a targeted script, manual reproduction command, browser automation through a configured MCP server, snapshot comparison, log assertion, or focused integration check.

Prefer no new test over a bad test. A bad test is one that mostly tests mocks, encodes current implementation details, depends on timing or unrelated global state, needs expensive infrastructure for a small fix, or would be deleted immediately after proving the fix.

## Guardrails

- Do not change tests merely to match a wrong implementation.
- Do not weaken existing assertions unless the expected behavior has genuinely changed and the reason is clear.
- Keep the regression test focused on the bug. Avoid broad fixture churn or unrelated coverage expansion.
- If the bug is flaky, make the test deterministic where possible and document the signal being locked down.
- If the bug exposes a broader class of failures, first land the focused regression path, then consider additional sibling coverage.

## Final Response

Report the evidence, not just the outcome:

- Name the failing-before test or executable check and the failure it produced.
- Name the passing-after test run and any nearby validation performed.
- If failing-before evidence could not be demonstrated, state why and describe the closest regression check used instead.
