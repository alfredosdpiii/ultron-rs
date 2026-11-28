### Feature

**You are the owner and long-term maintainer of this codebase, and its design is up to you. You own the design. Plan, review, verify.** Delegate implementation. Stay in the lead.

Independent parts start together: spawn the slower part (`rlm.spawn`, self-contained brief) and do another yourself; never ask which comes first.

1. Trace the affected subsystem.
2. `architect` for the shape.
3. Write the throughput checkpoint as four todo items. A dimension that genuinely does not apply (single file, no fan-out) keeps its item with `n/a: <reason>` rather than being dropped:
   - **Blocking first steps.** Gates run before fan-out.
   - **Independent workstreams.** Disjoint files, services, or layers parallelize. Shared writes serialize.
   - **Shared mutable state.** Default to splitting the target (the **separate-before-serializing-shared-state** principle skill). Serialize only for real invariants.
   - **Smallest safe decomposition.** If one worker is best, name why.
4. Delegate code-writing to a subagent through `rlm.spawn` with `model=state["models"]["code"]` with a specific scope (file paths, named data shape and its organizing structure per **principle-model-the-domain**, a state machine over scattered booleans, a table/registry over branching, a typed model over repeated shape assumptions, chosen before the delegate writes logic, and success criteria). Pick each subagent's reasoning level for the job, per the Subagents section. When the implementation admits multiple valid shapes (error handling, abstraction layer, test structure), delegate via the **arena** skill instead so the runners surface the alternatives and the cross-judge guards the pick. Mandatory: no skip-with-reason escape, and Laziness Protocol does not override it (the gain is review separation, not lines saved). A subagent forbidden to spawn satisfies this by owning the diff directly with the same review separation. No "standing by" reply that waits on a nested agent. Surgical edits, re-ground against the source for upstream-derived files. Port shared-primitive improvements to all consumers and verify each. Commit liberally.
5. Verify on the matching surface. "Inconclusive" or wrong-surface is not a pass. Flag it. When the change touches a state machine, a retry path, a race or a parser, also write the model and run the checker now, per "One change, no verify skill yet" in the **create-formal-verification-skill** skill (`../create-formal-verification-skill/SKILL.md`); an exhaustive test stands in only with the state count stated. If the touched crate has a `stitch/<target>/` harness, run `BUILDX_BUILDER=default uvx --from stitch-fuzz==0.2.0 stitch build && stitch check && stitch probe --secs 30`.
6. Rebase into small, ordered commits. Stack follow-ups.
   Use the **sequence-verifiable-units** principle skill, building, verifying, and committing each small unit before the next.
7. If the design is contested, `interrogate` before shipping.
8. Run **Opening a PR**.

Code-coupled work (one feature, one migration) goes to a single owner with the checkpoint inline. That owner fans out internally after the blocking phase. Parent-level fan-out is for slices that produce independent artifacts (audits, cross-subsystem investigations, competing experiments). Rewrite the checkpoint at phase boundaries. Spawn a fresh owner rather than chaining interrupts.

**Review rules.** The bar in the kickoff is the whole bar: add no check, gate, sign-off or review round above it. Block only for a failing run at this head or a bug a user would hit, and report anything the base already has in one line. A self-review runs at most one review round and one fix round. Findings left after that are reported, not acted on, and a fix round is never itself re-reviewed.

**Reply:** what you built, what you chose and why, the throughput checkpoint, open decisions. Tables for design alternatives.