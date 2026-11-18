### Bug fix

**You own this task. Plan, review, verify.** Delegate investigation and the fix to subagents, stay in the lead. Pick each subagent's reasoning level for the job, per the Subagents section.

Independent parts start together: spawn the slower part (`rlm.spawn`, self-contained brief) and do another yourself; never ask which comes first.

Be scientific. Every shipped line traces to runtime evidence. Belt-and-suspenders that "might help" is a hypothesis, not a fix. It does not ship. When evidence refutes a hypothesis, revert what it motivated. The smallest change the evidence justifies ships, nothing more.

1. Reproduce it yourself on the matching surface via the control skill (Non-negotiables), even when a debug or instrumentation protocol says to ask the user to reproduce. Ask the user only with a stated, specific reason the control surface cannot reach the target, and only after driving it as far as it goes. If it won't reproduce directly, synthesize the trigger, tighten conditions, or instrument until it fires.
2. Binary-search the cause. Form the candidate hypotheses, then rule them out until one survives. Seed them with a trace of the affected subsystem and the **why** skill for regression history. Each pass, take the split that cuts the most remaining problem space, get runtime evidence, eliminate. When program state is unclear, add instrumentation or logging and read it as the code runs. Don't guess. Drive a long or stubborn hunt with Cursor's `/goal` command. Confirm the surviving *mechanism* with runtime evidence before step 3's `architect` or an `interrogate` fan-out.
3. Plan the fix. If it crosses a function boundary, `architect` first. Delegate implementation to a subagent through `rlm.spawn` with `model=state["models"]["code"]` with a specific scope.
4. Verify on the same surface. The original repro now passes. "Inconclusive" or wrong-surface is not a pass. Flag it. Unit tests show branch behavior, not bug absence. When the change touches a state machine, a retry path, a race or a parser, also write the model and run the checker now, per "One change, no verify skill yet" in the **create-formal-verification-skill** skill (`../create-formal-verification-skill/SKILL.md`); an exhaustive test stands in only with the state count stated.
5. Stage the commits so the failing repro lands before the fix in git history. See the **tdd** skill for the failing-test-first cadence when the bug has a cheap local test path. Skip it when the test would be expensive, integration-heavy, or unclear.
   This is the canonical **sequence-verifiable-units** principle skill, the failing test first and the fix on top.
6. Run **Opening a PR**.

**Review bar.** The bar in the kickoff is the whole bar. Add no check, gate, sign-off or review round above it. Block only for a failing run at this head or a bug a user would hit, and report anything the base already has in one line. **Self-review.** At most one review round and one fix round. Findings left after that are reported, not acted on. A fix round is never itself re-reviewed. See `../worker.md`.

**Reply:** what was broken, root cause, fix, how you verified. Paste failing-then-passing repro output verbatim.