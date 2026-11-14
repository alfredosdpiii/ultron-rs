---
name: mission
description: "Run work too big for one context as a mission. One spec with acceptance items, features that each claim items, workers in their own worktrees held to host-run checks, and one validator per milestone, kept to check each fix, that exercises the merged result. Nobody is asked to approve a step. Use for /skill:mission, 'run this as a mission', or a request with several features that one session would not finish well."
disable-model-invocation: true
metadata:
  source: Ultron; the shape (a validation contract written before code, workers, validators that are not the worker) follows Factory's Droid missions, in Ultron's own words and APIs
---

# Mission

A mission is one request split into features, built by subagents and checked by other subagents. You are the
orchestrator: you plan, hand out work, judge what comes back and ship. You write no feature code yourself.

Nobody approves a step. Where the request leaves a choice open, pick, and record it with
`spec_api.decide(choice, reason, if_overruled)` so the pull request shows it. Stop for the user only when nothing can move without them (a credential, a paid call, a
destructive step).

Two rules carry the whole thing:

- **What done means is written before any code**, as acceptance items a check can show true or false.
- **Whoever built a feature does not decide that it works.** The host runs the worker's checks, and a validator that
  never saw the worker's reasoning exercises the merged result.

```python
state["plan"] = ["contract", "features", "build", "validate", "ship"]
```

## 1. Contract

Read the code the request touches first. Then write the spec: one sentence per behaviour, failure paths included
(a timeout, a cancelled call, a cleanup that fails halfway), each one something a command or a quoted line can settle.

```python
import spec_api
print(spec_api.write("Daily totals", "The sales page shows totals per day, in UTC.", [
    "A day with sales shows their sum",
    "A day without sales shows 0",
    "A totals query that times out shows the error banner and records the failure",
]))
```

The ids it prints (A1, A2, ...) are the contract. If the request changes later, write it again with
`revise="<what changed>"`; items whose text stays keep their result.

## 2. Features

Split the work into features that can be built apart. Every acceptance item belongs to exactly one feature, and a
feature with no item is not part of this mission. Group features into milestones: a milestone is a point where the
merged result can be run.

```python
import json
FEATURES = [
    {"name": "totals-query", "milestone": 1, "items": ["A1", "A2"], "paths": ["src/totals/**", "tests/totals/**"],
     "check": "pytest -q tests/totals"},
    {"name": "totals-timeout", "milestone": 1, "items": ["A3"], "paths": ["src/totals/**", "tests/totals/**"],
     "check": "pytest -q tests/totals/test_timeout.py"},
]
claimed = [i for f in FEATURES for i in f["items"]]
assert len(claimed) == len(set(claimed)), "an item claimed twice"
for f in FEATURES:
    await board.add(f["name"], detail=json.dumps(f))

async def features(milestone, status="open"):
    tasks = [t for t in await board.list(status) if t.get("detail", "").startswith("{")]
    return [f for f in ({**json.loads(t["detail"]), "task": t["id"]} for t in tasks) if f["milestone"] == milestone]
```

The board is the feature list from here on: it outlives a restart and the user can read it. A feature is `open`
until a worker has it, `claimed` while it is built and validated, `done` when every item of it passed, and `dropped`
only with a note that says why. Only you move a task: workers and validators report, they do not touch the board.

Features that edit the same files are one feature, or run one after the other.

## 3. Build

Baseline first. Every worker's contract rests on the test command running in a fresh worktree, so show that it does
before anyone builds on it: one worker that changes nothing, under a contract whose check is the repository's own
test command (the one its README or CI runs; a check may take 600 000 ms at most, so name the part that covers the
mission's paths when the whole suite takes longer).

```python
BASELINE = "pytest -q"
base = await rlm.spawn(
    f"Change nothing. Run `{BASELINE}`. If it fails, quote the first error and say whether the setup is at fault "
    "(a dependency or tool the worktree lacks) or the tests themselves.",
    name="baseline", worktree=True, contract={"checks": [{"run": BASELINE, "timeout_ms": 600_000}]})
```

End your turn; its end arrives as an event. The host ran the command and read the exit code, so the verdict is not
the worker's word.

```python
item = (await rlm.collect([base]))[0]
contract = (item["result"].get("check") or {}).get("contract") or {}
if contract.get("verdict") != "verified":
    print(contract.get("failed"), item["result"].get("summary"))
    fix = await board.add("baseline", detail=f"`{BASELINE}` fails before any feature: {contract.get('failed')}")
```

A failing baseline is the first task on the board, and no feature worker starts until it is `done`. When the
worktree lacks its setup, pass `worktree_setup={"command": "<install command>"}` to the baseline worker and to every
worker after it, and run the baseline again. When the tests themselves fail, one worker repairs them under the same
check (`worktree=True`, `contract={"checks": [{"run": BASELINE}]}`); merge and commit its work before step 3 goes on.
A baseline is repaired in the setup or the code: an emptied or skipped test, a rule switched off, or a narrower
command than the one that failed is not a repair.

Then one worker per feature, each in its own worktree, each held to a contract the host checks itself. The brief is all
the worker knows: give it the goal, the paths, the text of its items and its check command.

```python
WORKER = """
Rules: stay inside the paths above. Write the test for each item first and see it fail, then make it pass. Never
edit a test, an assertion or a snapshot to reach green. Run checks whole: no `| tail`, no `|| true`. If you cannot
check an item here, say blocked and why; blocked is not success. Finish with
rlm.finish(status, summary, evidence=[each command with its exit code], changed_files=[...],
left=[one string for each thing you left undone or found broken outside your paths])."""

def brief(f, found=""):
    items = "\n".join(line for line in spec_api.read().splitlines() if any(f" {i} " in line for i in f["items"]))
    return (f"Build `{f['name']}`.\nPaths: {', '.join(f['paths'])}\nIt is done when:\n{items}\n"
            f"Check: {f['check']}\n{found}{WORKER}")

todo = await features(1)
hs = await asyncio.gather(*[
    rlm.spawn(brief(f), name=f["name"], worktree=True, hop={"retry": 1},
              contract={"checks": [{"run": f["check"]}, {"changed_files_within": f["paths"]}]})
    for f in todo
])
for f in todo:
    await board.claim(f["task"])
```

End your turn; each worker's end arrives as an event. Then:

```python
done = await rlm.collect(hs)
good = []
for f, h, item in zip(todo, hs, done):
    contract = (item["result"].get("check") or {}).get("contract") or {}
    if contract.get("verdict") == "verified":
        good.append(h)
    else:
        await board.update(f["task"], status="open", note=f"contract failed: {contract.get('failed')}")
print(len(good), "of", len(hs), "verified")
merged = await rlm.merge(good, on_conflict="stop")
```

A worker whose contract failed after its retry is not merged, and its feature is open again with what the host saw.
Read the note, then either build it again with a narrower brief or mark its items `fail`. Do not fix it by hand and
call it built.

Before the next milestone, settle every `left` item the workers returned (`result["verdict"]["left"]`; the host
continues your turn once with them). Each becomes a new feature on the board, a
new acceptance item (`spec_api.write(..., revise="<what turned up>")`), or a task you add and move to `dropped` with
a note saying why it will never need doing. "Low priority" and "later" are not reasons: work that may need doing is
tracked.

## 4. Validate

`rlm.merge` leaves the merged work uncommitted. Read the merged diff, commit it (explicit paths), then run each
check on the committed tree, recorded: a check on uncommitted changes says nothing about what a pull request carries.

```python
for f in todo:
    print(await spec_api.check(bash, f["check"]))
```

Then one validator for the milestone, kept alive so it can check a fix against what it saw fail. It gets the items
and the entry point, not the workers' summaries: it must run the real thing (the CLI, the endpoint, the page) with
the default configuration, and report per item. A kept child works in your directory, so edit nothing while it runs.

```python
VALIDATE = """
You did not build this. For each item, exercise the behaviour through the real entry point and report
pass, fail or blocked with the command and what it showed. Do not edit source files. A check you could not run is
blocked, never pass."""
REPORT = {"returns": {"type": "object", "required": ["items"], "properties": {"items": {
    "type": "array", "items": {"type": "object", "required": ["id", "status", "evidence"]}}}}}
v = await rlm.spawn("Validate milestone 1.\n" + spec_api.read() + VALIDATE, name="validate-m1", keep=True,
                    contract=REPORT)
```

After it ends, record what it found. Only you mark items and move tasks.

```python
found = (await rlm.collect([v]))[0]["result"]["verdict"]["outputs"]["items"]
for item in found:
    print(spec_api.mark(item["id"], item["status"], item["evidence"]))
failed = {item["id"]: item["evidence"] for item in found if item["status"] != "pass"}
for f in await features(1, "claimed"):
    mine = {i: failed[i] for i in f["items"] if i in failed}
    await board.update(f["task"], status="open" if mine else "done", note=json.dumps(mine) if mine else "validated")
```

A feature with a failed item is open again, with the evidence as its note. Build it again as in step 3, with
`brief(f, found="The validator saw this fail:\n<evidence>\n")`, merge, commit and run its check. A kept child
takes no worktree, so the fix goes to a new worker, not back to the one that built it. The validator is the one
that remembers: ask the same one, and it checks the fix against the failure it saw and the rest for regressions.

```python
again = await rlm.ask(v, "The fix for A3 is merged and committed. Check A3 again and the other items for "
                         "regressions. Report every item as before.")
for item in again["verdict"]["outputs"]["items"]:
    print(spec_api.mark(item["id"], item["status"], item["evidence"]))
```

Go to the next milestone when every feature of this one is `done`.

## 5. Ship

Commit (explicit paths), then let the gate hold the result against the contract:

```python
import review_api
review = await review_api.run(rlm, "")
print(review.report)
path = spec_api.description()   # refuses with what is still open
```

`gh pr create --title "<type(scope): subject>" --body-file <path>`. The description is built from the spec, the
review's purposes and the recorded checks; add sections below its first line if a reviewer needs more. When
`description` reports more than one purpose, open one pull request per purpose instead of waiving it.

## What a mission is not for

A change one session can hold: use the playbook you were given, with the same `spec_api` calls. Reading, searching
or classifying: use `rlm.map`. A race between approaches: `/skill:arena`.
