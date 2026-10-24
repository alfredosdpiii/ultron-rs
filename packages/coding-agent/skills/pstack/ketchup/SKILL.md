---
name: ketchup
description: "Catch the user up on what changed in this session since their last message, in plain words and small visuals. List every item waiting on them with what they need to decide it from the reply alone. Use for /skill:ketchup."
disable-model-invocation: true
license: MIT (see ../LICENSE)
metadata:
  source: github.com/cursor/plugins/pstack/skills/ketchup
  author: Lauren Tan
  modified: ported to Ultron's REPL
---

# Ketchup

Catch the user up on what changed in this session since the last message they wrote before `/skill:ketchup`, and help
them understand each change, not only where it stands. Then lay out every call waiting on them so they can make it from
your reply alone. Runtime events, child results and messages from other agents are not their messages. Assume the user
remembers their last message and nothing after it.

Use the voice of `../bro/SKILL.md`, not its task: plain words, short sentences, no jargon, like one person catching up
another. Say what changed for the user, not how you did it. Say "the tests pass", not "CI is green". Leave out hashes,
file paths, commands, build numbers and test counts unless the user needs one to act. Call each PR, branch, child or job
by a name the user would know, not by a label you invented. If a fact needs context to make sense, give the context or
leave the fact out.

## Gather what changed

Keep this turn read-only. Make visuals only from what you already have: do not change code, start an app, a child or a
job, or post anything. Collect your own work since that message, results from children, and messages from anyone else
in the session. Then check the current state of each PR, CI run, child, task or job this session touched, in one cell.
Nothing here waits: never `rlm.collect`, which blocks on running children.

```python
import json
user = (await ctx.history(limit=200, kinds=["user"]))["items"]  # the last one is this /skill:ketchup message
since = user[-2] if len(user) > 1 else None
print("since:", f'{since["id"]} {since["preview"][:200]}' if since else "(no earlier user message in view)")

async def safe(name, call):
    try:
        return name, await call
    except Exception as error:  # an API this host does not offer, or a failed read: a gap to report
        return name, f"unavailable: {error}"

status = dict(await asyncio.gather(
    safe("children", rlm.results()),            # never waits: ended results plus ids still running (Rust host)
    safe("subagents", rlm.list_subagents()),    # every child with its state, on any host
    safe("tasks", agents.tasks()),              # typed agent tasks
    safe("background", background.list()),      # background jobs
    safe("shell jobs", rlm.jobs()),             # bash(..., yield_after=...) jobs
    safe("goal", goal.get()),                   # the /goal and where it stands
    safe("board", board.list()),                # the session's task board
    safe("inbox", swarm.inbox(mark=False)),     # unread agent messages, left unread
))
for name, value in status.items():
    print(name, json.dumps(value, default=str)[:600])
```

For each PR or branch the session touched, read its state through `bash` (`gh pr view <n> --json
state,mergeStateStatus,reviewDecision,statusCheckRollup`, `gh pr checks <n>`, `git log -1 <branch>`). A child that
succeeded counts as verified only when `result["check"]["outcome"] == "verified"`; say so for the rest. Say which
items are still running and which you could not check, and why. Check each number you will show against the facts it
comes from, and say so when you correct one.

An action item is a call or step only the user can take, such as a product choice, an approval, an irreversible action,
or access you lack. Include any still waiting on the user, even one raised before their last message. Work you can
still do yourself is not an action item. Never make up a call to fill the reply. A step the user did not ask for and the
work does not need is not an action item, unless it is irreversible and waiting on the user: give it one line and no
visual.

One call can cover many items, such as each PR in a set awaiting merge. For each item, collect its link, what it changes
for users, the evidence behind your pick, its risk, and your pick. Write "unknown" for a fact the call needs that you do
not know. Leave out a link you do not have.

## Write the reply

Spend words on the calls, not on the story. Keep what happened under 100 words, not counting pictures, and shorter when
less happened. Leave out work that changed nothing for the user. Give each item about 30 words. The reply grows with the
number of items: two items need well under 150 words in all, not counting pictures, and thirty need far more. Never cut
items or shrink them to stay under a word count: no count in place of a list, and no link in place of an item. A link
can add depth, but the reply alone must be enough to decide.

Open with one short sentence that names what waits on the user, or says nothing does. The action items give the detail.
If nothing happened and nothing waits on the user, say so and stop.

Then show what happened, pictures first: the diagrams that explain a change, and the screenshot and recording files the
work produced, named by path. Then sum up in one short bullet per thing the user asked for: what came of it, and whether
it is done, still running, failed, or not checked. Write each line the way you would say it out loud: "Search was slow
from a missing index", not "Search review: done" or "Slow search: missing index". Name any bug found and fixed on the
way. Each line must stay true when read alone. Never mark something done that still waits on the user, and leave out
anything the action items say. Under the summary, give at most three short lines that change what the user knows or
must do. Then name in one line any work left that you can do yourself.

Then list the action items. Ask the call the user owns, such as merge, approve, or pick, and give your pick. A step you
would take first goes in your pick. So does anything that must happen first, and anything that cannot be undone. Give
the number or link of each PR, issue, child or job a call is about, also in a call between options.

Fit the shape to the call. A call about one item is one bullet: the call, your pick, what it changes for users, the
evidence behind your pick, and its risk, with the item named once. When items share a call, write the call and your
pick once, then give each item its own bullet, riskiest first: its link and name, what it changes for users, the
evidence behind your pick, and its risk. Name an item's pick only where it differs from the call's. Give each item's
risk even when it is low, such as "Low risk: only the colors change." Say what waiting costs only when it is more than a
delay, such as a deadline or lost data.

For a call between options, give every option the work weighed, each with what it gets the user and what it costs, and
the current state when there is one. Those costs are the call's risk, so add no separate risk line. When the
screenshots above show the options, write each option's gain and cost beside its screenshot's path.

## Visuals

A visual is a picture: a Mermaid diagram drawn with `await diagram.mermaid(text)`, which shows inline in the
terminal, or a screenshot, recording, chart or generated image saved to a file and named by its path. A Markdown table
is not a visual, even with before and after columns.

When something is easier to grasp as a picture than as sentences, draw it: how a design or system works, a PR stack or
dependency chain with each PR's state, a before and after of a fix, or options side by side. Never draw a status list,
a list of action items, or boxes with nothing connecting them. Never give a fact twice, in text or across visuals.

Keep each visual small enough to read at a glance: at most six boxes, with at most eight words in each. Write labels as
plain words a person would say, not tags like "Tests: pass". A fact that needs a sentence belongs in the text, or
nowhere.

Name the screenshot or recording of each visible change by its path. If a visible change has none, mark its summary
line "No screenshot yet," and say there what changed on screen. Draw diagrams and charts from facts you have: a Mermaid
diagram for how things connect, and for numbers a Mermaid `xychart-beta` or a chart saved as a PNG. Never invent a
number, and never draw or generate a picture of a screen you did not capture. Point only to files and PRs that exist.
