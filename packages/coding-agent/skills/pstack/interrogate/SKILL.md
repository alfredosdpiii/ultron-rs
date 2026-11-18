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

Adversaries try to break a change or a design. A finding survives only when a run proves it. Report the findings. Do not apply fixes.

## Frame

The scope is what the user names. Otherwise it is the branch diff from its merge base with the base branch, through the working tree. State the intent with the **in-your-own-words** skill (`../in-your-own-words/SKILL.md`), from the user's message, the commits, and the PR description. Ask the user when the intent is unclear.

## Fan out

Pick the reviewers with `rlm.find_models`, one per model family where possible. Spawn each adversary as a `worktree=True` child, so each starts from your tree as it is now. Give each the scope, the intent, and this brief:

> Break this change. Prove each finding with a run against the real code, such as a failing test, a repro script, or command output that shows the wrong behavior. For a design or plan, run a prototype or probe the current tree. Keep repros in your own worktree. Report each finding with a title, the exact command, the output it printed, and the output the intent requires. Leave out anything you did not run. That covers style, naming, structure, and risks that need a future change to matter. No findings is a valid result.

```python
hs = [rlm.spawn(f"Scope: {scope}\nIntent: {intent}\n\n{BRIEF}\n"
                "When done, call rlm.finish with each finding in outputs={'findings': [{'title', 'command', 'output', 'expected'}]}. "
                "Do not fix anything and do not commit.",
                name=f"interrogate-{i}", model=m, worktree=True)
      for i, m in enumerate(models)]
results = await rlm.collect(hs)
```

Check each result's `check["outcome"]`; trust a finding only from a child whose check is "verified".

## Verify

Rerun each finding's command yourself, in the adversary's worktree or a scratch copy. Keep a finding when the output shows the failure and the failure breaks the intent. Drop the rest. Merge findings that share a root cause. Do not run a judge or pick one adversary's list: different adversaries find different failures, so the verdict keeps every verified finding.

## Output

- **Intent**: the restatement.
- **Findings**: for each verified finding, the command, its output, how it breaks the intent, and the adversaries that found it. Say "no verified findings" when none survive.
- **Dropped**: for each adversary, the count of dropped findings and why.
