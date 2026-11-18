---
name: how
description: "Use for \"how does X work\", code walkthroughs before changing something, and placement / ownership / layering questions (\"where should this live\", \"which package owns this\", \"is this the right layer\"). Explains subsystem architecture, runtime flow, onboarding mental models. Use why for motivation."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/how
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# How

Explore the codebase to answer "how does X work?" questions.

If the scope is ambiguous, state your interpretation and explore. The user can redirect.

## 1. Explore and explain

Read the code yourself with `rg`, `sed -n` and `rlm.load` over large files. Trace the flow end to end before you describe it. If you cannot trace a part, say so: "I couldn't determine how X connects to Y" beats a guess.

For a question that spans several packages, you may hand the explanation to one read-only child with `rlm.spawn`. Give it the question, the paths you already found, and this brief: write an architectural explanation a senior engineer unfamiliar with this area could read and walk away with a solid mental model from. The child must not edit files and returns through `rlm.finish`. Collect it with `await rlm.collect([h])`, then check the claims you will repeat against the code yourself.

## 2. Present

Write the explanation with the sections that apply: Overview, Key Concepts, How It Works, Where Things Live, Gotchas. Use a mermaid diagram when components talk to each other or data moves through stages; skip it when prose covers the flow. Light edits for clarity are fine. Do not substantially rewrite a child's output.
