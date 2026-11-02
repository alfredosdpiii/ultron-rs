---
name: dream
description: "Nightly consolidation for the durable instance. Reads recent sessions, autoreview lessons and Hindsight, and keeps only what evidence backs as global memories and tested code skills, each change reversible and reported. Runs on its own from the dream schedule; use /skill:dream to run one now."
disable-model-invocation: true
metadata:
  source: Ultron; Prime Agent's continual harness learning, Phantom Guardrails (2607.13083), /retro (2610.34938)
---

# Dream

You run unattended, at night, in the durable instance's dream session. Nobody will answer a question: decide,
record why, and move on. Your job is to turn the last day of experience into a small number of durable lessons
that will make tomorrow's sessions better, and to retire lessons that turned out wrong.

The rule that governs everything: **no lesson without evidence, no change without a way back.** A lesson cites
what happened (a session file and a quote from it, a review finding and its fate, a fact id). A lesson you cannot
quote is a guess; drop it.

## 1. Gather (read only)

Keep raw text in variables; only findings reach your context.

- **Sessions since the last dream.** `runpy.run_path` the recall skill's `scripts/sessions.py` (`pstack/recall` in
  the bundled skills directory), then `session_files(days=1, all_projects=True)`; skip this session, subagent
  sessions (`parent` set) and sessions with no user message. Look for: a correction the user made, a command that
  failed then worked, an approach the user rejected, a procedure repeated by hand in two or more sessions.
- **Autoreview.** Each reviewed repository has a learned guide (Hindsight mental model `autoreview-<id>`) and raw
  finding fates in the `ultron-autoreview` bank. Read with `bash` and `curl` against the Hindsight URL
  (`hindsightUrl` setting, default `http://localhost:8888`): `GET /v1/default/banks/ultron-autoreview/memories/list`
  and `GET .../mental-models`. Read only; never write to that bank.
- **Memory.** `await memory.prepare("<theme>", scope="global")` for each theme you found, so you see what is already
  kept, and `await memory.reflect("<question>")` to test a hunch against everything the bank holds.
- **Last dream's report** (step 4) and `await skills.code_list()`: what earlier dreams added.

Use `rlm.map` frames for long sessions, one session region per frame, with a contract such as
`{"lesson": str, "kind": str, "quote": str, "file": str}`. Quotes must be copied, not paraphrased; check each one
with `quote in text` in Python before you trust it.

## 2. Judge

Keep a candidate only if all hold:

- It recurred (two or more sessions or reviews), or the user stated it outright once.
- It is not already kept (the recall in step 1 shows nothing that says the same).
- It changes what an agent would do next time. "The user worked on X" is a progress note, not a lesson.
- It holds outside the one project, for a global memory. A lesson about one repository belongs to that project's
  own sessions, which learn it there; leave it.

A kept memory that the day's evidence contradicts is a candidate to retire.

At most five new memories and one new code skill per night. Fewer is normal; none is fine.

## 3. Act (every change reversible)

- **Memory:** `await memory.propose(text, evidence, scope="global", type=..., key="dream-<kebab-topic>",
  situation=...)`. The key makes a later dream replace the record instead of piling up near-duplicates. `evidence`
  is a list of `{"ref": "<session file or fact id>", "quote": "<copied text, at most 300 bytes>"}`.
- **Retire:** `await memory.invalidate(fact_id, reason)` for a fact the day's evidence shows is wrong, quoting the
  evidence in the reason. For an observation it retires nothing and returns `{"state": "observation", "sources"}`:
  invalidate the wrong source facts and Hindsight rewrites the observation from the rest.
- **Code skill:** only for a procedure done by hand in two or more sessions, small enough to test without the
  network: `await skills.propose_code(name, source, test_source, evidence)`. It activates only if its test passes;
  `skills.rollback(name, version)` undoes it.
- Never edit files in a repository, never push, never message anyone, never write to the autoreview bank.

## 4. Report

Write `~/.ultron/agent/dreams/<YYYY-MM-DD>.md` (create the directory): one line per change with its kind (memory,
retired, code skill), its key or name, the evidence refs, and how to undo it (`memory.forget(<id>)`,
`skills.rollback(<name>, <version>)`). Then a short list of candidates you dropped and why. If nothing changed,
the report says so in one line. End your turn with the same summary.
