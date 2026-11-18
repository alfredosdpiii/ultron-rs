---
name: why
description: "Use for 'why does X work this way', 'why we picked Y', design rationale, regressions, postmortems, or data-backed thresholds. Searches git history and every configured MCP evidence source (issue tracker, docs, team chat, observability, error tracking, analytics warehouse), then returns a cited, confidence-tiered read on decisions and tradeoffs. Use how for runtime behavior."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/why
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Why

Answer from the record, not the code: read the commits and PR discussions that introduced the target and each later change, and other sources if the trail is thin. Cite what the authors wrote, mark your own inferences as inferences, and say what the record does not answer.

## Gather

Work in your REPL. No subagents: this is reading and searching.

- Source control: `git log --follow`, `git blame -L`, and `git log -S` for the exact constant or string, through `bash`. Read the PRs behind the merge commits with `gh pr view <n> --json title,body,comments,reviews`.
- Other sources: `await mcp.servers()` lists the configured MCP servers. Map each to one category (issue tracker, documents, team chat, observability, error tracking, analytics). Search each with `await mcp.search(...)` and `await mcp.call(tool, ...)`. An error such as auth required is a gap to record, not a finding.
- Keep the anchor, the sources searched (queries included) and the hits in `state["why"]` so they survive a restart.
- Large results: narrow with `h = await rlm.load(text=...)` and `h.search(...)` before you read them.

## Synthesize

One `rlm.infer` frame writes the answer from the gathered record. Pick a strong model with `await rlm.find_models("opus")` when one exists. Then check it yourself: open every cited PR, commit, ticket or message you are unsure of and confirm it says what the answer claims. Fix or downgrade what fails.

```python
found = await rlm.find_models("opus", limit=1)
model = f"{found[0]['provider']}/{found[0]['id']}" if found else None  # None: the settings default
answer = await rlm.infer(task, context=[json.dumps(state["why"], indent=1)], contract=str, model=model)
```

Present the answer with its sources, one line per category, including the empty and unavailable ones with the reason.
