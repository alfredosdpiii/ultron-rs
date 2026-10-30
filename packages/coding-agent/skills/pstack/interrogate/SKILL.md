---
name: interrogate
description: "Adversaries try to break a change or design, and only findings proven by a run survive. Use for \"interrogate\", \"adversarial review\", \"multi-model review\", \"challenge this\", \"stress test this code\", \"find blind spots\", or \"tear this apart\"."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/interrogate
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Interrogate

Adversaries try to break a change or a design. A finding survives only when a run proves it. Report the findings.
Do not apply fixes.

## Frame

The scope is what the user names. Otherwise it is the branch diff from its merge base with the base branch, through
the working tree. State the intent with the in-your-own-words skill
(`../in-your-own-words/SKILL.md`), from the user's message, the commits, and the PR description. Ask the
user when the intent is unclear.

## Fan out

Run the arena skill's runner pick, per-candidate worktrees and fan-out (`../arena/SKILL.md`, Phase A steps 3-4 and
Phase B), with two changes. The scope's uncommitted changes are already in each worktree: an Ultron worktree branches
from your tree as it is now. In place of the arena task and rationale, give each adversary the scope, the intent, and
this brief:

> Break this change. Prove each finding with a run against the real code, such as a failing test, a repro script, or
> command output that shows the wrong behavior. For a design or plan, run a prototype or probe the current tree. Keep
> repros in your own worktree. Report each finding with a title, the exact command, the output it printed, and the
> output the intent requires. Leave out anything you did not run. That covers style, naming, structure, and risks that
> need a future change to matter. No findings is a valid result.

```python
BRIEF = "..."  # the brief above, verbatim
SCOPE = "..."  # what the user named, or the diff range
INTENT = "..."  # the restatement from Frame
FINISH = """
When done: await rlm.finish(status, summary,
evidence=[<each run you cited, as "command: outcome">],
outputs={"findings": [{"title": "...", "command": "...", "output": "...", "expected": "..."}]},
changed_files=[]). Do not fix anything; do not commit; your branch is committed for you."""

hs = await asyncio.gather(*[
    rlm.spawn(f"Scope: {SCOPE}\nIntent: {INTENT}\n\n{BRIEF}\n{FINISH}", name=f"interrogate-{i}",
              model=m, worktree=True)
    for i, (m, item) in enumerate(state["arena_runners"])
])
```

## Verify

Verify each finding like the arena's Phase F: rerun its command yourself, in the adversary's worktree or a scratch
copy. Keep a finding when the output shows the failure and the failure breaks the intent. Drop the rest. Merge
findings that share a root cause.

Skip the arena's judge, base pick and graft phases: adversaries find different failures, so the verdict keeps every
verified finding, not one adversary's list.

## Output

- **Intent**: the restatement.
- **Findings**: for each verified finding, the command, its output, how it breaks the intent, and the adversaries that
  found it. Say "no verified findings" when none survive.
- **Dropped**: for each adversary, the count of dropped findings and why.
