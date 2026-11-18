---
name: architect
description: "Sketch types, signatures and module structure before code, run an arena of structurally distinct designs, then implement against the chosen sketch and scrap it when it proves wrong. Use for /skill:architect, 'architect this', 'design this', or non-trivial work where jumping to code would lock in the wrong shape."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/architect
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Architect

Before writing code, trace every system the change touches, then sketch two different shapes for it (types, signatures, module boundaries) and pick the one where each piece of state has one owner, each task has one way to do it, and callers need to know least. Redesign instead of patching if workarounds repeat.

For a large or contested design, run the **arena** skill (`../arena/SKILL.md`) instead of sketching alone. Its runners are `rlm.spawn` children, each in its own worktree, and the synthesis is checked before you use it.

Keep the sketch as files in the repo (types, signatures and a short rationale). Implement against it. If the implementation keeps fighting the sketch, re-sketch instead of patching around it. For a contested sketch, run **interrogate** (`../interrogate/SKILL.md`) before you build on it.
