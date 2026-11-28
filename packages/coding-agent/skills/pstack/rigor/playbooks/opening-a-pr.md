### Opening a PR

Run at the end of every other playbook that changes code.

Work from a git worktree off trunk, and keep unrelated work out of it. Children that edit files get `worktree=True` and come back through `rlm.merge`. Rebase into small, landable commits. Strip slop from the diff, then `../no-comments/SKILL.md`, then `../unslop/SKILL.md`.

**Self-review.** Run `r = await review_api.run(rlm, "main")` after `import review_api`, then `print(r.report)`. Fix what holds up in one fix round (if a finding suspects a panic, bounds, or UTF-8 bug in a parser/stateful API, prove or refute it with `playbooks/fuzzing.md` / STITCH++). Then stop: findings left after that are reported in the PR, not acted on, and the fix round is not reviewed again. Only that one review round runs, however many children the change used.

**Title.** `type(scope): subject`, naming a real symbol when one carries the change. No trailing period.

**Body.** A one-minute briefing under 40 lines, with these `##` headings in order, dropping empty ones: `## Why`, `## What changed`, `## Scope`, `## Tradeoffs` (only rejected alternatives a reviewer would otherwise ask about), `## Blast Radius`, `## Verification`. Write it with `../technical-writing/SKILL.md`. Evidence logs and long tables go in a linked file, not the body.

**Stacks.** Stack follow-ups by base branch. Each child branch targets its parent branch (`gh pr create --base <parent-branch>`). Keep a stack to three or four PRs.

**Readiness.** Open PRs ready for review, never as a draft (omit `--draft`; `gh pr ready <number>` if one opens as draft). Opening a PR does not start a babysit.

**Review rules (the self-review above is the one review round).** The bar in the kickoff is the whole bar: add no check, gate, sign-off or review round above it. Block only for a failing run at this head or a bug a user would hit, and report anything the base already has in one line. A self-review runs at most one review round and one fix round. Findings left after that are reported, not acted on, and a fix round is never itself re-reviewed.
