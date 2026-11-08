# Ultron

**A terminal coding agent that programs its way through your work instead of picking tools from a menu.**

Most coding agents give the model a list of tools (read a file, run a command, edit, search) and let it call
them one at a time, pasting every result into its context. Ultron gives the model one thing: a persistent Python
REPL. Shell commands, edits, file reads, your MCP servers, sub-agents and sub-model calls are all Python functions
inside it. The model writes a cell that loops, filters, fans out and keeps the results in variables, and only what
it prints comes back into its context.

Ultron is one self-contained Rust binary. It needs no Node.js: it embeds its runtime files (the Python REPL runtime,
bundled skills, docs and changelog; see `crates/cli/build.rs`) and needs only Python 3 at run time. It is a fork of
[Pi](https://github.com/badlogic/pi-mono) and keeps Pi's interface: the TUI, `-p` print mode, `--mode json`,
`--mode rpc`, providers, skills and prompt templates. It adds an
[Agent Client Protocol](#editors-ultron-acp) server for editors, a durable instance that keeps working after you
close the terminal, and it can run with [Claude Code](https://claude.com/claude-code) as the model underneath or
inside Claude Code as its only tool. Pi extensions and Pi packages are not supported.

```bash
tar -xzf ultron-<version>-<target>.tar.gz
./ultron-<version>-<target>/install.sh     # into ~/.local/bin
ultron setup
```

## Which mode should I use?

Ultron runs in three ways. All three use the same runtime: the REPL, handles, frames, sub-agents, jobs, Loki and
budgets. What differs is whose interface you see and which model drives the root agent.

| | `ultron` | `ultron --claude` | `ultron claude` |
|---|---|---|---|
| You see | Ultron's TUI | Ultron's TUI | Claude Code's TUI, plus `ultron watch` for the RLM view |
| Root model | any provider: API key, subscription login, or a custom OpenAI-compatible endpoint | Claude on your Claude Code login (`claude-subscription/claude-opus-5-5` by default), with Anthropic API credits as a fallback when the plan's window runs out | Claude Code on your login, `claude-opus-5-5` by default |
| Who manages the context | Ultron (`ctx.*`, compaction, results that collapse to one line) | Ultron | Claude Code |
| A finished job or sub-agent wakes the model | yes | yes | interactive: yes, through a background hook; `-p` and `--no-hooks`: no, the event arrives with the next `rlm` result or your next message |
| Ultron's panels and commands (`/review`, `/settings`, `/rlm`) | yes | yes | no (`ultron watch` shows the RLM view) |
| Use it when | you use a non-Claude model, or want every Ultron feature | you want Claude on your Pro or Max plan in Ultron's UI | you prefer Claude Code's own UI and keybindings |

The [Claude Code](#claude-code) section has the details of both Claude modes, and of using Claude Code only for
sub-model frames.

## What makes Ultron different

### 1. The REPL is the only tool

The model's tool list is exactly one entry, `rlm`. Everything else is a pre-imported async function:

```python
out = await bash('''pytest -q tests/test_parser.py''')          # shell, output as a Python string
await edit("src/parser.py", old_str=OLD, new_str=NEW)           # exact edit that fails loudly if stale
runs = await asyncio.gather(*(mcp.call("exa-agent_exa_agent_create_run", query=q)   # your MCP servers
                              for q in questions))
```

With separate tools available, models take the familiar route (a shell call, a file write) and never use the REPL.
On the TypeScript product's hard task set, REPL use went from 0 of 16 runs to 29 of 30 once the REPL was the only
door. This is the design of Prime Intellect's RLM harness, including its choice to expose MCP tools as Python skills.
The Rust build has no switch back to separate tools: `ULTRON_TOOLS=native` and `ULTRON_EXTENSION_TOOLS=native` are
reported at startup and ignored.

### 2. Big inputs never enter the model's context

```python
h = await rlm.load("logs/app.log")                    # a handle: size, digest, never the text itself
hits = h.search(r"ERROR .*timeout", limit=50)
causes = await rlm.map("Root cause in 10 words.",      # one bounded sub-model call per slice
                       [h.lines(m["line"] - 20, m["line"] + 5) for m in hits],
                       contract=str, budget=Budget(calls=60))
```

`rlm.infer` and `rlm.map` run private sub-model frames that see only the slices you pass them, return values
validated against a JSON-schema contract (malformed answers are re-asked), and draw on one shared budget of calls,
tokens and depth. Running out returns an `Incomplete` that carries its evidence. Frames can run on a different,
cheaper model than the root (see [Models](#models-settings--models)).

`budget.estimate(plan)` prices planned work (sub-agents, frames, chats, judge calls) before it runs and says whether
it fits what is left of `ULTRON_MAX_COST_USD`; an unknown price is reported as unknown. With a cost cap set, work
whose estimate would cross it is refused before it starts, and `workflows.run` returns its estimate with its result.

### 3. Long work never blocks the turn

A shell command or tool call still running after 30 seconds (`ULTRON_BASH_YIELD_AFTER`) becomes a background job
with a handle. When it finishes, a short `<runtime_event>` message wakes the model. It never polls, sleeps or stares
at a progress bar, and a test suite can run while it fixes something else.

A cell that hangs (a `git push` waiting for input) never locks the REPL. The next call runs in a side lane with its
own kernel, its result says which cell is still running, and `await rlm.cancel(cell_id)` stops the stuck one along
with every process it started.

Work can also outlive the turn. `/goal <objective>` hands an objective to a background job with its own REPL and no
wall-clock limit; it ends the goal with `goal.complete(...)`, which needs the host's run of `/goal check <command>` to
exit 0 when one is set. `schedule.every("1h", prompt)` and `schedule.at(when, prompt)` let the root agent send its
session a prompt later (see [The durable instance](#the-durable-instance-and-scheduled-prompts)).

### 4. It has a runtime, not just a loop

- **Sub-agents with their own kernels** (`rlm.spawn`), typed agents with validated inputs and outputs
  (`agents.invoke`), and agent graphs with joins and bounded revision loops (`workflows.run`).
- **Checked sub-agent verdicts.** A sub-agent ends with `rlm.finish(status, summary, evidence=..., changed_files=...)`.
  `passed` needs concrete evidence (a test command and its result, or file lines), and a malformed verdict is sent
  back to be fixed. The host snapshots the workspace before and after the child runs. If a file the child says it
  changed did not change, the verdict is `contradicted`, and a workflow node with a contradicted verdict fails. A
  child that ends without a verdict keeps its result but is marked unverified.
- **Contracts the host checks itself.** `rlm.spawn(brief, contract={"checks": [...], "returns": schema})` gives a
  child up to 16 checks that the host runs when it ends: a command and its expected exit code or output, files it may
  change (`changed_files_within`), `no_new_dependencies` and `files_exist`. A failed check fails the result, and
  `hop={"retry": N}` runs the child again with the failures, or `hop={"review": brief}` starts a reviewer sibling.
  Contracts survive a worker restart.
- **Briefs that stay small.** `rlm.spawn(brief, context="retrieve")` starts a child that is told how to search the
  parent's session instead of carrying it; `context="fork"` carries the whole conversation, and `"fresh"` is the
  default. `rlm.progress()` reports how running children are doing (starting, exploring, changing, checking,
  finishing, stalled), with cost and a forecast to their timeout, without a model call.
- **Past sessions are searchable.** `sessions.list`, `sessions.search` and `sessions.read` find and reopen spans of
  this project's sessions (or every project's, with `cwd="all"`). `/continue [session id]` starts a new session that
  searches the one it left instead of carrying a summary of it.
- **Nesting is opt-in.** A sub-agent cannot spawn its own sub-agents unless it was started with `depth=N`, and no
  chain goes deeper than 3 levels (`ULTRON_SPAWN_DEPTH`). The limit also holds across Claude Code child processes.
- **Agents as Python classes**: the docstring is the prompt, `...` methods are model-driven with typed returns
  checked by the host, and fields are durable state.
- **Code skills and procedure skills.** A procedure that worked is saved as Python with a test
  (`skills.propose_code`), and only goes live when the test passes; versions roll back. A SKILL.md can also declare a
  `procedure:` of segments (a goal, the files each may write, a check command, a judge question or an agent), and
  `skills.run(name, input=...)` runs it with the host deciding every step: a write outside a segment's files fails
  it, and the final check runs again before a run passes. The bundled `tdd` skill is a procedure. Proposed procedure
  skills wait for you: `ultron skills proposals | show <id> | approve <id> | reject <id>`, which ask at a terminal and
  refuse inside a model's cell.
- **The model manages its own context** (`ctx.forget`, `ctx.summarize`, `ctx.pin`, `ctx.note`), and finished task
  results collapse to one line after the model has seen them.
- **Durable state**: a task journal, per-request caps on tokens (`rootBudget.maxTotalTokens`, off unless set), turns,
  wall time (30 minutes by default) and cost, call ceilings per kind (`ULTRON_MAX_TASK_CALLS`,
  `ULTRON_MAX_MODEL_CALLS`), a memory cap over the kernel's whole process tree, and kernel snapshots that survive
  restarts. A cell cut short by a restart says its effect is
  unknown instead of being replayed.
- **Checkpoints of the working tree.** Before each user turn, the session records the working tree as an
  unreferenced Git commit (or a jj snapshot); HEAD, branches, the index and your files are untouched. `/tree`
  (Shift+R) and the Esc-Esc rewind selector restore the files of an earlier turn, and restoring the tip again undoes
  the restore.
- **A kernel that stays up.** The kernel speaks its protocol on private pipes (fds 3 and 4). Anything a cell writes
  straight to stdout or stderr (`os.write`, subprocesses, C extensions) becomes cell output instead of breaking the
  connection. Each cell's stdout and stderr are capped at 4 MiB each, and output over the model's budget is saved
  whole to a file the truncation marker names.

#### Sub-agents in their own Git worktrees

Children that edit files at the same time can overwrite each other's work in a shared checkout.
`rlm.spawn(brief, name=..., worktree=True)` gives a child a private Git worktree instead. It is branched from the
parent's tree as it is at that moment, uncommitted changes included, on `ultron/<session>/<name>`. It lives under
`.git/ultron-worktrees/`, so the parent's tools and `git status` never see it. The parent's index and files are not
touched.

- The child's kernel, `bash`, `edit`/`write`/`read`, jobs, Loki checks and verdict check all run in the worktree.
  An absolute path into the parent's checkout (as briefs often give them) is mapped to the worktree's copy. Files
  the child still changes in the parent's tree show up as `check["outside"]`.
- When the child ends, its changes are committed on its branch. The commit message is its verdict's summary, with
  no hooks, no signing and no trailers. Its result gains
  `worktree: {branch, path, commit, changed_files, diffstat}`. A child that changed nothing has its worktree and
  branch removed right away.
- `await rlm.merge(hs)` merges each child's branch into the parent's working tree in order, three-way, against a
  snapshot of the tree as it is now. The changes land as uncommitted edits (new files untracked). Nothing is
  committed on your branch: you review and commit. The first conflict stops the merge and writes nothing of that
  child: the report names the files and hunks, and the tree stays as the earlier merges left it.
  `on_conflict="skip"` goes on with the next child, and `"markers"` writes conflict markers for you to resolve.
  Children that did not pass are skipped unless `include_failed=True`. Merged and empty children's worktrees and
  branches are removed; conflicted or failed ones are kept for inspection.
- Group before spawning: put coupled tasks (the same function, or one building on another) in one child's brief,
  and give independent tasks their own children. Every worktree child gets its own worktree; worktrees are never
  shared. A child's own children branch from its worktree and merge back into it. `workflows.run` nodes take
  `worktree: true`, and Claude Code children (`ultron claude`) run in their worktree too.
- Git ignores the parent's `node_modules`, `.venv` and `.env`, so a new worktree would not have them. Gitignored
  `node_modules`, `.venv` and `venv` directories are linked in by default, and small `.env` files are copied.
  `worktree_setup={"link": [...], "copy": [...], "command": "npm ci"}`, or the `worktrees` setting (`link`, `copy`,
  `setupCommand`), changes this. Linked dependencies are shared, so children must not install different ones at the
  same time. A package installed in editable mode (`pip install -e .`) still imports the parent's sources, and
  dev servers or databases that children start share the parent's ports.
- Cleanup: when a session ends, its finished children's worktrees are removed and their unmerged branches are kept.
  Worktrees of a crashed session are pruned when the next session starts in the repository. Uncommitted work is
  committed to the branch first, and only Ultron's own worktrees and branches are touched.
  `await rlm.worktrees.list()` and `await rlm.worktrees.cleanup(branches=False)` manage them by hand, and
  `ULTRON_KEEP_WORKTREES=1` keeps everything.
- Limits: this needs Git and a repository with at least one commit. `worktree="auto"` falls back to the shared
  tree when either is missing. Submodules are not initialized in a worktree, and Git LFS files are checked out only
  if git-lfs is installed.

### 5. Memory you can check

Long-term memory lives in memorantado or Hindsight, and the model uses it from the REPL: `memory.prepare(query)`
recalls, `memory.propose(text, evidence)` keeps a fact, `memory.correct` and `memory.forget` fix it. A write that
holds a secret or credential is refused before it reaches the store. Records can be typed (`type=` preference, fact,
procedure, pitfall or reference, with a one-line `situation`), and a record with `path_globs` is pointed at, in one
line, when a cell writes a matching file; the model reads it with `memory.get(id)`.

- **Backends.** `memory.backend` chooses `memorantado` or `hindsight`. Unset, a memorantado that answers at
  `http://127.0.0.1:3789` (`memorantadoUrl`, `ULTRON_MEMORANTADO_URL`) when a session starts becomes the backend and
  stays it; until then, Hindsight at `http://localhost:8888`. Once memorantado is the backend, a session where it
  is down pauses memory with a notice and writes nothing to Hindsight, so memories never split across two stores.
  `ultron memory migrate --to memorantado [--dry-run]` copies Hindsight's memory into memorantado once; running it
  again copies only what changed.
- **Automatic memory.** When you set the memory server explicitly (`hindsightUrl`, `ULTRON_HINDSIGHT_URL`, or
  memorantado chosen or given a URL), or turn on `memory.autoRecall` and `memory.autoRetain`, the session recalls
  before each user message of three words or more (at most 6 memories, with a 5-second limit) and opens each context
  with the project's and the user's brief. It keeps compaction summaries and, when a session closes or has been quiet
  for 30 minutes, the user's messages and the agent's final replies of the part no summary covered. Recalled
  memories arrive as a message on the branch, never in the system prompt, so the prompt cache holds. Without an
  explicit server or those settings, nothing is recalled or kept unless the model asks.
- **Dreaming re-checks what it kept.** `memory.dream(days=7, limit=50)` confirms memorantado claims whose quoted
  files still hold their quotes without a model call, asks the [judge](#6-a-judge-seat-for-closed-questions) about
  changed quotes (revising, retracting or leaving them for review), and flags claims that contradict each other.
  `memory.brief` writes the startup brief and `memory.topic` topic pages. The durable instance runs a dream every
  night at 03:30 (`learn.dreamAt`; `learn.dream: false` turns it off) and skips nights when no session changed.
- **Imports.** `ultron memory import --from claude-code|codex [--dry-run]` turns Claude Code's per-project memory
  notes or Codex's memory summary into typed records; a note holding a secret is refused, and running it again
  replaces only changed notes.

### 6. A judge seat for closed questions

`await judge.decide(state, questions, kind=...)` answers closed questions about evidence (a choice, yes or no, a
score, a number) for batch and background checks: dreaming, autoreview's blind check, `knowledge.relations`. It asks
TypeSafe's Jev first, then a self-hosted d1 server (`judge.d1.url`), then `judge.fallbackModel`, and abstains when
none answers. An answer below its confidence threshold (`judge.thresholds`; 0.75 for Jev and 0.85 for d1 by default)
or one that disputes what the caller expects goes to `judge.escalateModel` when that model is of another family than
the one that answered, and is otherwise marked for review.

- Limits: `judge.budgetUsd` caps a session's judge spend, `judge.maxCalls` its calls (1000 by default), and every
  call is a usage row of the session.
- Keys: `TYPESAFE_API_KEY` and `ULTRON_D1_KEY`, or auth.json's `judge:typesafe` and `judge:d1`. A stored key goes only
  to the origin it was saved for, any key goes over plain http only to loopback, and secrets are redacted from what
  the judge sees.
- `ultron judge bench <cases.json> [--backend jev|d1|fallback|escalate]...` asks each backend every case of a
  labelled file and prints accuracy per kind, latency (p50, p95, max), tokens and cost. An example cases file is
  [`crates/judge/tests/data/judgebench.json`](crates/judge/tests/data/judgebench.json).
- `ultron optimize judge` replays a recorded live run, fits `judge.thresholds` per question kind and backend, and
  writes them as a proposal file; `ultron optimize memory` and `ultron optimize autoreview` measure variants of scoped
  recall and of autoreview's blocking level. Each prints quality, penalty and cost with the Pareto frontier marked,
  and none changes a setting.

`ultron setup` has a Judge step, and `/settings` lists the backends, budget, fallback and escalation models. See
[settings](packages/coding-agent/docs/settings.md#judge-seat-ultron).

### 7. You can see it think

- **The RLM graph** draws the run as a live tree: turn, cells, tool calls, sub-agents, `rlm.map` fan-outs with
  progress bars, workflows and jobs, with budget gauges and a kernel strip. Ctrl+R docks it and Alt+G opens it
  full screen, where you can step into any node and see its input, result and the Python call that fetches it.
- **The RLM pane** (Alt+W on terminals 120 columns or wider) splits the screen: sub-agents and workflow nodes as
  boxes in dependency waves, with arrows to the nodes that depend on them, and cards showing each node's model, latest
  text, time, turns and tool calls. It opens by itself, without taking the focus, when a turn spawns children, runs a
  workflow or fans out an `rlm.map` (once per turn; close it with q and it stays closed until the next one; turn it off
  with the `rlmPaneAutoOpen` setting or `ULTRON_RLM_PANE_AUTO=off`).
- **Diffs of what a cell wrote.** Files a cell changed through `write()` and `edit()` are listed under the expanded
  cell with their diffs (Ctrl+O).
- **Diagrams in the transcript.** `await diagram.mermaid(text)` renders Mermaid to SVG and PNG, shows the PNG to you
  and gives the model only its path. It needs Node.js on the session's `PATH`.
- **The session report** (`/usage`, `ultron usage`) answers "did this session use depth, or only the root?" without
  a script over the session file: turns and wall time, cells and the REPL APIs they named, frames and sub-agents
  (by model, with verdict checks, worktree branches and merges), tokens and cost per lane kind and per model, what the
  guardrails did (Loki, masked secrets, hints, steers, refused work) and the memory store's operations. `/usage`
  reports the running session; `ultron usage [session-id|path]` reads any session file offline (no model call, no
  server, nothing written), `--last N` prints one line per recent session (`--here` for this directory only), and
  `--json` is the same report for scripts (`ultron.session-report/1`). It works for `ultron`, `ultron --claude`,
  `ultron claude` and durable sessions; a number a session did not keep is printed as `not recorded`, and a cost
  nobody reported as `unknown`, never as zero.

  ```text
  $ ultron usage
  Session 01a0fae6-d1e8-716d-b0c4-c136bcf29a2a  [ultron]
  ~/work/demo  ·  2026-10-02 08:37 to 2026-10-02 08:37  ·  348.8 kB

  Depth      depth 2: 4 frames, 4 sub-agents (1 nested)
  Turns      1 turn, 1s wall
  Root       scripted/scripted (4 responses)
  Cells      6 cells, 1 failed
             bash 1 · write 1 · rlm.infer 1 · rlm.map 1 · rlm.spawn 2 · rlm.collect 2 · rlm.merge 1
             root 3 · sub-agents 3
  Frames     4: 4 complete · 906 tokens
             from 1 rlm.map call and 1 rlm.infer call
             scripted/scripted  4 frames  906 tokens
  Sub-agents 4: 4 completed · max depth 2 (1 nested)
             verdicts: 2 verified · 0 contradicted · 2 unverified (2 without a verdict)
             scripted/scripted  4 sub-agents  18.7k tokens
             worktree ultron/01a0fae6/writer: 1 file, merge merged
  ...
  ```

  This report is of a scripted test session, whose model has no price.
- **Request traces.** `ULTRON_REQUEST_TRACE=<file>` appends one line per model request with what changed since the
  previous one, and `ultron trace check <file>` lists every prompt-cache reset with its reason (a model switch,
  compaction, a `ctx.forget`, a changed system prompt). It exits 1 when there was a reset.
- **REPL result footer.** `rlm.resultFooter` (off by default; "REPL result footer" in `/settings`) ends each REPL
  result the model sees with `[t+... ctx ...]`: the time since the session started and the context size.

### 8. Reviews that check their own findings (`/review`)

`/review` is an optional command for reviewing a change: the working tree plus the current branch by default, or
`/review main`, `/review 123` (a PR, through `gh`), `/review src/`. Five specialist reviewers (correctness,
security, architecture, tests, AI/LLM integration) run as bounded `rlm.map` frames over the diff in chunks with the
surrounding code; then every finding goes to a verifier frame that reads the lines it cites and the callers of the
code involved. Rejected findings never reach the report, confirmations must quote the source, and the report ends
with counts, cost and what was not checked. One token cap covers it all (`--budget`, default 300k); `--only sec,bugs`,
`--model`, `--deep` (a sub-agent re-checks undecided findings) and `--post` (only after you say yes) adjust it.
It adds nothing to the prompt of ordinary turns.

#### Automated pull-request reviews (`ultron autoreview`)

`ultron autoreview run` reviews a pull request as soon as one of your logged-in `gh` accounts is requested as
reviewer or @mentioned on it, and posts the review under that account: an acknowledgement comment when it starts,
then one review with a few inline comments on the diff, each tagged with its level (critical, high, medium, low,
nit), and a verdict (request changes from medium up, approve, or a plain comment when coverage was incomplete).

- **A deterministic stage runs first.** Before any model finding is verified, every diff is checked by plain code:
  Python compiles in process, JSON and TOML parse, `node --check`, error-class `ruff` and `mypy` when they are on
  `PATH`, and Loki's core rules. Its problems on added lines are posted as confirmed findings, and a model finding it
  contradicts (a syntax error in a file that compiles) is dropped.
- **Then the finder-then-verifier pipeline** runs directly, with no root model turn, and a deep pass follows the
  change beyond the diff (callers, helpers, tests, claims in comments) through read-only repository lookups, every
  finding backed by quoted source the host has checked.
- **A blind check.** Each confirmed finding the run did not prove (at most 12 per review) goes to the
  [judge](#6-a-judge-seat-for-closed-questions) with only the claim, the diff hunk and the source, never the finder's
  reasoning or the verifier's verdict. A refuted finding is posted as uncertain. Without a judge backend nothing
  changes.
- **Re-reviews.** Asked again after a push (or after requesting changes), it reviews only what is new, re-checks its
  earlier findings, and resolves the threads whose finding is fixed.
- **It learns from what people do with its findings.** Each posted finding is settled as accepted or rejected by
  what GitHub shows, and `ultron autoreview register [--repo owner/name]` shows the failure-mode register: recorded
  reviews and their fates, the plain-code checks that caught their planted bug, and the failure modes rejected
  findings keep showing. Review rules that findings keep citing and people keep accepting are written as Loki rule
  proposals to `autoreview/loki-proposals.json`; nothing is applied.

`--dry-run` writes the would-be review instead of posting, `ultron autoreview review owner/repo#N` reviews one pull
request now, `ultron autoreview eval <repo-dir>` measures the reviewer on a repository's history, and
`ultron autoreview install` writes a user service (systemd, or a launchd agent on macOS). `/autoreview on|off` does
the same from the TUI. Pull request content is sent to the configured model provider. Details:
[`packages/coding-agent/docs/autoreview.md`](packages/coding-agent/docs/autoreview.md).

### 9. Guardrails on by default

- **[Loki](#loki-guardrails)** checks every file Ultron writes (hardcoded secrets, injection sinks, protected files,
  new linter and type-checker findings) and blocks a bad write before it happens.
- **Secret masking.** Cell output is scanned before it reaches the model or the transcript. API keys, tokens,
  private keys, credentials in URLs and similar values become `[REDACTED:<kind>]`; the values in the kernel are left
  as they are. `ULTRON_MASK_SECRETS=off` turns it off. The rules are
  [`secret-patterns.json`](packages/coding-agent/src/ultron/rlm/secret-patterns.json).
- **The profile is off limits to the model.** `write()`, `edit()` and shell commands cannot change the profile's
  `settings.json`, `auth.json`, `skills/`, `skill-proposals/`, `memory-backend` or `optimize/`, whatever Loki's mode.
- **A stuck-loop detector**, off by default (`ULTRON_HINTS=on` turns it and the other cell hints on), watches for
  three failing cells in a row, the same two cells alternating, or eight failures in one turn. The first time, it
  asks the model for three hypotheses and one cell that tests the likeliest; the second time, it tells the model to
  stop repeating the approach and say what blocks it.

## The durable instance and scheduled prompts

A plain `ultron` uses its project's instance: a server under `~/.ultron/projects/<name>-<hash>/` with its own
sessions, keyed by the repository's top level (or the directory outside Git). `ultron durable` uses one global
instance in `~/.ultron/server` whose server outlives its clients, so kept children and scheduled prompts keep running
after you close the terminal. `ultron instances` lists the running servers of both kinds. Auth, settings, skills,
themes and models are shared.

```bash
ultron durable                      # the TUI, attached to the durable server (also -p and --mode json|rpc)
ultron durable start|status|stop
ultron-s                            # the durable instance's home session, in $HOME, reused across clients
ultron-s --new | --list | --session <id> [--home]
ultron service install              # run the durable server as a systemd user service, started at login
ultron service status|restart|uninstall
```

`ultron-s` is a link to `ultron` that `install.sh` creates. A client killed mid-run leaves the run going, and the
next client continues after it. An idle durable server without schedules makes no provider requests and starts no
worker. The service unit keeps no keys: the server uses the profile's stored credentials and the optional
`<agent dir>/service.env` (`KEY=value` lines) for environment variables such as provider keys. `install.sh` restarts
an installed service so it runs the new binary.

Scheduled prompts are set from a cell by the root agent: `schedule.every(interval, prompt, name=..., until=...,
max_runs=...)` (at least 60 seconds, at most 365 days) and `schedule.at(when, prompt)`. Each fire arrives in the
session as a prompt and starts a run, waiting for a running one to end. Schedules fire only while a server of the
instance runs: a server that holds a pending schedule stays up, and a server that starts later fires what it missed
once. `/schedules` and `ultron schedule list` show them, `ultron schedule cancel <id|name>` cancels one, and the
`ERROR` column of the list shows a failed delivery, which is retried every 60 seconds. With `ultron service install`,
schedules of durable sessions keep running across logouts and reboots.

## How it compares

| | Typical tool-calling agent | Ultron |
|---|---|---|
| Model's interface | a menu of tools, one call per step | one persistent Python REPL |
| Large files and outputs | pasted into context | handles; only printed slices come back |
| Many independent steps | many model turns | one cell with a loop or `asyncio.gather` |
| MCP servers | separate tools | Python functions in the REPL |
| Slow commands | the turn waits | background jobs, completion events |
| Reading lots of text | the model reads it all | bounded sub-model frames under a budget |
| Delegation | sub-agent tools, if any | sub-agents with checked verdicts and host-checked contracts, typed agents, workflows, agent classes |
| Work after you leave | stops with the session | a durable instance with scheduled prompts and goals |

## Results measured on the TypeScript product

Every number below was measured on the TypeScript releases of Ultron named in each heading, before the Rust
rewrite. The Rust product has not been re-measured on these task sets. The result files are in the TypeScript
repository at tag v0.87.39:
[`acceptance/quality/`](https://github.com/alfredosdpiii/ultron/tree/v0.87.39/acceptance/quality). All of these are
small samples (one or two trials per task), so read them as indicative.

### SWE-bench Verified (TypeScript release 0.87.22)

A seeded 50-task sample of [SWE-bench Verified](https://www.swebench.com/), one run per task, every agent on
`gpt-6.1-sol` at medium reasoning and given only the issue text, working inside the task's official container and
scored by the official evaluation
([results](https://github.com/alfredosdpiii/ultron/blob/v0.87.39/acceptance/quality/2026-10-03-swebench-verified-50-v0.87.22-cliproxyapi_gpt-6.1-sol.md)):

| | Resolved | Wall time, 50 tasks | Tokens | Notional cost |
|---|---|---|---|---|
| **Ultron 0.87.22 (TypeScript)** | **44/50 (88%)** | **72 min** | **2.69M** | **$2.37** |
| Codex CLI 0.152.1 | 44/50 (88%) | 100 min | 9.43M | $4.33 |
| Pi 0.84.4 | 43/50 (86%) | 83 min | 3.16M | $2.84 |

Accuracy is a tie: a one-task gap at n=50 is noise. Ultron resolved as many tasks as Codex with 29% of its tokens and
28% less time. Ultron used no frames or sub-agents on these tasks; they are single-file fixes. On the same first 10
tasks on Claude Opus 5.5 through Claude Code
([results](https://github.com/alfredosdpiii/ultron/blob/v0.87.39/acceptance/quality/2026-10-03-swebench-verified-pilot10-claude-opus-5-5.md)),
`ultron --claude` and plain Claude Code each resolved 9/10 with the same verdict on every task, Ultron with 27% of
the tokens (328k against 1.21M) and 47% of the notional cost. That `ultron --claude` ran Claude Code's own loop; the
Rust build runs Ultron's loop on the subscription instead.

### Against Pi

Measured with
[`scripts/eval-quality.mjs`](https://github.com/alfredosdpiii/ultron/blob/v0.87.39/scripts/eval-quality.mjs)
against stock Pi on the same model.

**glm-5.3-flash**, thinking `max`, 2 trials per task (TypeScript release 0.87.12):

| Task set | Pi | Ultron |
|---|---|---|
| Hard set: 15 tasks (multi-file bugs, refactors, large data, log forensics) | 28/30, median 116 s, 2.53M tokens, 288 tool calls | 28/30, median 128 s, **1.96M** tokens, **182** tool calls |
| Research pilot: expired-certificate incidents among 157 reports | 1/2 (one run lost to a provider rate limit), 377 s | **2/2**, 397 s, 157 sub-model frames |
| Parallel work: a 150 s test suite plus two bug fixes | 2/2, median 175 s | 2/2, median 196 s |

**gpt-6-sol**, default thinking, 2 trials per task, both agents with an isolated home (TypeScript release 0.87.10;
rows marked 0.87.11 on that release):

| Task set | Pi | Ultron |
|---|---|---|
| Hard set: 15 tasks (0.87.11) | 29/30, median 34 s, 696k tokens, $1.11 | **30/30**, median 36 s, **586k** tokens, $1.02 |
| Research: 400 reports, 2.1 MB | 2/2, median 76 s, 638k tokens | 2/2, median **73 s**, **372k** tokens |
| Parallel work | 2/2, median 162 s | 2/2, median 162 s |
| Delegation: 6 services, bugs readable from the code, 300 s budget | 2/2, 119 s and 145 s | 2/2, **113 s and 78 s**, 6 sub-agents each |
| Delegation, deep: 6 services, bugs only a slow harness reveals, 300 s budget (0.87.11) | 6/6 both times, but **over budget**: 355 s and 366 s, $0.47 avg | 6/6 both times, **within budget: 165 s and 176 s**, $0.63 avg, 6 sub-agents each |
| Delegation, tickets: 12 small tickets in shared files, one coupled pair, 300 s budget (1 trial) | 10/12 (the coupled pair failed), 85 s, $0.04 | **12/12**, 148 s, $0.07, no sub-agents |

On the deep delegation task each service hides three bugs behind a 21-second test harness that stops at the first
failure, so working through six services one at a time cannot fit the budget. Ultron split the work into one
sub-agent per service on its own and finished in less than half of Pi's time for about 1.35x the cost; Pi fixed
everything too, sequentially, and ran over the budget in both trials.

### With Claude Code underneath (one trial each)

**`ultron --claude` against vanilla Claude Code**, both on Claude Opus 5.5, on the 8-task fast check of the
[AI Workflow Benchmark](https://github.com/alfredosdpiii/ai-workflow-benchmark) (TypeScript release 0.87.17,
[results](https://github.com/alfredosdpiii/ultron/blob/v0.87.39/acceptance/quality/2026-09-30-awb-fast-check-claude-opus-5-5.json)):

| | Claude Code | `ultron --claude` |
|---|---|---|
| Mean score | 78.1 | **93.8** |
| Tasks passed | 3/8 | **4/8** |
| Wall time, all 8 tasks | 747 s | **566 s** |
| Notional cost reported by Claude Code | $3.53 | **$2.28** |

The +15.6-point lift (2 tasks better, none worse, 6 tied) is not significant at n=8 (p=0.50).

**`ultron claude` against plain Claude Code**, both on Sonnet (TypeScript product,
`acceptance/quality/2026-09-29-*-claude-code_sonnet.json`):

| | Claude Code | `ultron claude` |
|---|---|---|
| Hard tasks (3) | 3/3, median 16 s, 189k tokens | 3/3, median 18 s, **60k** tokens |
| Research pilot (25 incidents to find) | precision and recall 1.0, 26 s, 268k tokens | precision and recall 1.0, 31 s, **93k** tokens |

Same accuracy for about a third of the tokens. Most of the difference is the system prompt, which is sent with every
request: Ultron's guide was about 3.9k tokens against Claude Code's 19k. Neither run used a frame or a sub-agent.

## Install

Requirements: **Python 3** for the REPL (`ULTRON_PYTHON` if set, else `/usr/bin/python3` on Linux, else `python3` on
`PATH`). No Node.js. The Claude modes also need the [Claude Code](https://claude.com/claude-code) CLI, installed and
logged in; `diagram.mermaid` and the bundled skills that ship a Node CLI need Node.js on `PATH`.

From a release archive (built by `scripts/rust-release.sh`):

```bash
sha256sum -c ultron-<version>-<target>.tar.gz.sha256
tar -xzf ultron-<version>-<target>.tar.gz
./ultron-<version>-<target>/install.sh                  # ~/.local/bin/ultron and the ultron-s link
./ultron-<version>-<target>/install.sh --prefix /opt/ultron
ultron setup                                            # provider and model, memory, judge, Loki
```

`install.sh` replaces the binary atomically (a running `ultron` keeps its old file), puts the license files in
`<prefix>/share/doc/ultron`, and restarts an installed `ultron service`.

From source:

```bash
cargo build --release -p cli         # target/release/ultron
ln -s ultron target/release/ultron-s # optional: the durable home session
target/release/ultron setup
```

The first run that needs them installs the runtime files into `~/.ultron/agent/runtime/<version>-<hash>/` (under
`$ULTRON_CODING_AGENT_DIR` when set); each release gets its own directory, and older ones are removed once no
running `ultron` uses them.

`ultron update` asks the npm registry for the latest `ultron-agent` version and, when it is newer, prints the GitHub
releases page to download from; it does not replace the binary, so updating means installing the new archive. `ultron update --models` refreshes the model catalogs.
Ultron also checks for a newer version at startup; `ULTRON_SKIP_VERSION_CHECK=1` (or `--offline`) turns that off.

Settings, credentials, skills and models live in `~/.ultron/agent`, separate from Pi's `~/.pi/agent`, so both can be
installed side by side. `ultron migrate import-pi <session.jsonl>` imports a Pi session tree,
`ultron migrate export-pi` writes one back, and `ultron migrate backup [dir]` / `restore <dir>` back up and restore
the profile (sessions excluded).

### `ultron setup`

A terminal wizard, also offered the first time `ultron` starts interactively without a usable model. Every step can
be skipped with Esc, nothing is overwritten without asking, secrets are typed masked and saved readable only by you
(mode 0600), and it can be run again at any time.

1. **Environment**: checks `python3` (and Docker, optional), with fixes when one is missing.
2. **Provider and model**: sign in with a subscription or save an API key (Pi's `/login`), or add a custom
   OpenAI-compatible endpoint to `models.json`; pick the default model and thinking level; then one tiny live
   request shows whether it works, with the provider's error if not.
3. **Memory backend**: memorantado or Hindsight.
4. **Hindsight**: finds a running server, starts it with Docker (`ghcr.io/vectorize-io/hindsight`, asking for the
   LLM key Hindsight itself needs), saves a different URL, or shows the manual install.
5. **Judge**: the judge seat's backends and their keys.
6. **Loki guardrails**: shows the bundled Loki version and which analyzers it would use are missing (with install
   hints), and turns auto-install, auto-commit and advise-only mode on or off.
7. **Summary** of what changed and where it lives.

## Quick start

```bash
cd your-project
ultron                                   # interactive TUI
ultron -p "why does test_parser fail?"   # one-shot print mode
ultron --claude                          # the same TUI, with Claude (Opus 5.5) on your Claude Code login
ultron claude                            # Claude Code's own TUI, with Ultron's REPL as its only tool
ultron --mode rpc                        # Pi-compatible JSONL RPC
ultron acp                               # Agent Client Protocol on stdio, for editors
ultron-s                                 # the durable instance's home session
ultron usage --last 5                    # what recent sessions did: depth, cells, tokens, cost
```

`ultron setup` configures a provider. Pi's ways work too: `/login` for subscription providers, or
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` and the like (`ultron --help` lists them all). Custom
OpenAI-compatible endpoints go in `~/.ultron/agent/models.json`.

Useful keys: Up/Down and Alt+R for prompt history, Ctrl+R for the RLM panel, Alt+W for the RLM pane, Alt+G for the
full-screen graph, Ctrl+O to expand cells and help. `/hotkeys` lists them all, and `/usage` reports what the session
did.

### Signing in from Docker or over SSH

A subscription login in the browser (Claude Pro/Max, ChatGPT, Radius) ends with the browser being sent to a callback
server that Ultron runs on `localhost` (ports 53692, 1455 and 1456). When Ultron runs in a Docker container or over
SSH, the browser on your computer cannot reach that server and ends on "This site can't be reached: localhost refused
to connect". On macOS and Windows that is also the case with `docker run --network host`, which joins the network of
Docker's VM instead of your computer's (unless Docker Desktop's host networking is enabled). The login has not failed
at that point:

- **Paste the address of that page.** Copy the failed page's full address from the address bar
  (`http://localhost:1455/auth/callback?code=…&state=…`) and paste it into the login prompt in the terminal, which
  is still waiting.
- **Or publish the callback port** and let the callback server listen on the container's interface instead of
  loopback only, which a published port cannot reach:
  `docker run -p 127.0.0.1:1455:1455 -p 127.0.0.1:53692:53692 -e ULTRON_OAUTH_CALLBACK_HOST=0.0.0.0 …` (ChatGPT
  and Claude; without `--network host`).
- **Or sign in without a callback:** a device-code login (GitHub Copilot, xAI, Kimi, Meta), an API key
  (`docker run -e ANTHROPIC_API_KEY …`, or "Use an API key" in `ultron setup`), or an OpenAI-compatible proxy on the
  host such as CLIProxyAPI, added as a custom endpoint in `ultron setup`.

### Running several sessions

Any number of `ultron` processes can run side by side, TUIs and `-p`/`--mode json`/`--mode rpc` runs alike, even
with different models, flags or environment (`ULTRON_LOKI=off`, `ULTRON_RLM_FRAME_MODEL=…`). Each process runs its
own server generation in the project's instance; starting one never disconnects another or stops its turns and
background jobs. A session lives in one process at a time: opening a session that another `ultron` still has open
fails with "is open in another Ultron process" instead of taking it over.

If a server or session worker is lost (a crash, a kill), the TUI says so: the session file is saved, so `ultron -c`
resumes it. Sub-agents that were running are reattached when the session's worker restarts; background shell jobs
that were running in the lost worker are gone.

### Models (`/settings → Models`)

Every model Ultron uses can be picked in the TUI while a session runs. `/settings → Models` has one row per role, each
with a picker over the models your providers offer:

- **Session model**: the root agent's model. Changing it switches the running session and saves it as the default,
  as `/model` does.
- **Frame model** (`rlm.frameModel`): code-free `rlm.infer` and `rlm.map` frames that name no `model=`.
- **Review model** (`review.model`): `/review` frames. By default it follows the frame model.
- **Sub-agent model** (`rlm.childModel`): `rlm.spawn` sub-agents. Only tool-capable models are offered.
- **Frame thinking** (`rlm.frameThinking`): off, low, medium or high.
- **Judge fallback model** and **Judge escalation model**: the judge seat's chat models.
- **Claude Code mode**: the root, frame and sub-agent models of `ultron claude`, and the model of its Ultron
  sub-agents.

The session, frame, review, sub-agent and judge rows apply to the running session: the next frame, review or
sub-agent uses the new value. The Claude Code mode rows apply the next time `ultron claude` starts. When an
environment variable (for example `ULTRON_RLM_FRAME_MODEL`) sets a role, its row says so and stays locked until you
unset it. See [settings](packages/coding-agent/docs/settings.md#models-of-rlm-work-ultron).

### Editors (`ultron acp`)

`ultron acp [session options]` serves the Agent Client Protocol (v1, JSON-RPC on stdio), so editors that speak it
(Zed, CodeCompanion.nvim, avante.nvim) drive Ultron. Each cell is reported as one tool call with its code and output,
and files a cell changed come back as edits with diffs. Neovim setup:
[docs/rust-rewrite/acp-neovim.md](docs/rust-rewrite/acp-neovim.md).

## The REPL at a glance

| | |
|---|---|
| `await bash('''cmd''')`, `await edit(...)`, `await read(path)` | shell, exact edits, file text (a handle for large files) |
| `await view_image(path_or_figure)` | show the model a screenshot, diagram or matplotlib figure |
| `await mcp.call(tool, **args)` | MCP servers |
| `await rlm.load(...)`, `rlm.infer(...)`, `rlm.map(...)` | handles and bounded sub-model frames |
| `await rlm.spawn(task, depth=0)`, `agents.invoke(...)`, `workflows.run(...)` | sub-agents, typed agents, agent graphs |
| `rlm.spawn(task, name=..., worktree=True)`, `await rlm.merge(hs)` | a sub-agent in its own Git worktree, and merging its work back |
| `rlm.spawn(task, contract=..., hop=...)`, `rlm.progress()` | host-checked contracts, progress without a model call |
| `rlm.finish(status, summary, evidence=...)` | a sub-agent's checked verdict |
| `@agent class ...` | agents as Python classes |
| `ctx.*`, `skills.propose_code(...)`, `skills.run(...)`, `memory.*` | context control, tested code skills, procedure skills, memory |
| `sessions.search(...)`, `sessions.read(...)` | past sessions |
| `judge.decide(...)`, `budget.estimate(...)` | closed questions to the judge seat, pricing work before it runs |
| `knowledge.relations(...)` | what code reads (settings, environment variables), which routes it serves, which tables it touches, which files tests import |
| `schedule.every(...)`, `schedule.at(...)` | prompts the session sends itself later |
| `diagram.mermaid(...)`, `rlm.images.generate(...)` | rendered diagrams, generated images (saved to files) |
| `state` | a dict that survives kernel restarts |

`knowledge.relations` settles what plain code can and asks the judge about the rest, caching answers per file
version. With `graph.neo4j.url` set, accepted relations (and memorantado's memories, when it is the backend) are
mirrored to Neo4j; see [settings](packages/coding-agent/docs/settings.md#knowledge-graph-mirror-ultron).

MCP servers are built in: list them in `~/.ultron/agent/mcp.json` (or `ultron mcp add <name> -- <command>`), manage
them with `/mcp` and `ultron mcp list|login|logout`, and call them from Python. They connect on first use, so they
never slow startup. See [docs/mcp.md](packages/coding-agent/docs/mcp.md).

## Claude Code

Ultron works with the [Claude Code](https://claude.com/claude-code) CLI in three ways:

- as the root agent's model in Ultron's TUI (`ultron --claude`);
- as the host, with Ultron's REPL as its only tool (`ultron claude`);
- as a provider for sub-model frames (`claude-code/*`).

All three run on your Claude Code login (for example a Pro or Max subscription) and check it with
`claude auth status`. Ultron never reads Claude credentials; the CLI logs itself in.

Driving Claude Code headlessly with `claude -p` (stream-JSON in and out, credentials left to the CLI) is the approach
of [xmpuspus](https://github.com/xmpuspus); Ultron's Claude Code integration follows it.

Claude Opus 5.5 (`claude-opus-5-5`) is the default model of both Claude modes, for the root, the frames and the
sub-agents. Under `ultron --claude`, frames and sub-agents follow the session model unless you set them in
[`/settings → Models`](#models-settings--models).

### Ultron's UI with Claude underneath: `ultron --claude`

`ultron --claude` is Ultron as usual (its TUI, agent loop, compaction, runtime, RLM pane, Loki, budgets and session
files) with the root agent's model calls made through your Claude Code login.

```bash
ultron --claude                           # claude-subscription/claude-opus-5-5
ultron --claude --model claude-code/haiku # another Claude model (sonnet, opus, haiku, a full id)
ultron --claude -c                        # continue the last session
ULTRON_ROOT=claude ultron                 # the same as --claude (or the global setting claudeCode.root: true)
```

How it works: the root lane runs on the `claude-subscription` provider. Each model call is one `claude -p` process
(one turn, stream-json in and out, no native tools, settings sources, slash commands or session persistence), and
Ultron's harness keeps the loop: it sends the transcript, gets one answer back, and runs each `rlm` call as an
ordinary tool call on the root kernel. The model's tool list is exactly `[rlm]`, advertised through an MCP inventory
server that executes nothing; a request declaring any other tool is refused. Sub-agents that inherit the root's model
run the same way on their own lanes and kernels, and tool-free frames use the plain `claude-code` provider.

When the plan's five-hour or weekly window runs out mid-session, the same request goes to Anthropic's API with your
configured key (for example the monthly API credits of a Max plan's linked Console organization), and the transcript
notes the switch; the first turn after the reset tries the subscription again. `claude.apiFallback: false`, or no API
key, keeps the subscription's error instead.

### Claude Code's UI with Ultron's REPL: `ultron claude`

`ultron claude` runs Claude Code as Ultron's root agent: Claude Code keeps its TUI, its model and your login, and its
only tool is Ultron's REPL.

```bash
ultron claude                             # Claude Code's TUI; the REPL (rlm) is its only tool
ultron claude --watch                     # inside tmux: the RLM view opens in a split beside it
ultron watch                              # the RLM view of the running session, in another terminal
ultron claude -p "why does test_x fail?"  # print mode; any other argument goes to claude
ultron claude --print-config              # the exact claude command and configs, for other tools
ultron guide --for claude                 # the system prompt Claude Code gets
```

How it works: `ultron claude` checks `claude auth status` (it never reads Claude Code's credentials or settings
files) and starts `claude` with `--strict-mcp-config --mcp-config` pointing at `ultron mcp`, `--tools ""
--allowedTools mcp__ultron__rlm` (no built-in tools, no permission prompts for the REPL), `--system-prompt-file`
with Ultron's guide in place of Claude Code's default prompt, `--setting-sources ""` (your Claude Code settings,
hooks and CLAUDE.md are not loaded; `--keep-settings` loads them, `--keep-mcp` keeps your MCP servers),
`--dangerously-skip-permissions` (no permission prompts; `ULTRON_CLAUDE_SKIP_PERMISSIONS=off` keeps them, and it is
left out when running as root unless `IS_SANDBOX=1`), and a temporary `--settings` file with Ultron's hooks. Its
Claude Code subagents run the same way. `ultron mcp` is an MCP server that runs Ultron's own runtime, so you can also
add it to any Claude Code setup yourself (`claude mcp add ultron -- ultron mcp`, then allow `mcp__ultron__rlm`).

The system prompt (and each Claude Code subagent's) includes your AGENTS.md/CLAUDE.md context files and your skills,
loaded exactly as native `ultron` loads them (same settings and project trust); `--no-context-files` and
`--no-skills` leave them out.

**The same as native Ultron:** the persistent kernel and its skills, output truncation, secret masking, Loki
guardrails, `rlm.load`/`infer`/`map` frames, subagents with checked verdicts, typed agents, workflows, shell jobs, the
usage ledger, kernel snapshots, working-tree checkpoints, and memory. Each Claude Code session has its own Ultron
session, found again by Claude Code's session id, so `claude --resume` reopens the same tasks, `state` and ledger.

**Different:**

- Claude Code owns the conversation: its context, compaction and model loop. `ctx` context edits and Ultron's own
  TUI panels are not available; `ultron watch` shows the RLM view (graph, waves of subagents, frames, jobs,
  verdicts, kernels and Loki stats) in a second terminal, or in a tmux split with `--watch`.
- Waking: in an interactive session, an async hook installed after each `rlm` call waits in the background and wakes
  Claude Code when a job, subagent or task ends while it is idle. With `-p` or `--no-hooks`, the event is reported at
  the top of the next `rlm` result or with your next message.
- Hooks: SessionStart adds Loki's note; UserPromptSubmit delivers waiting events; Stop closes the turn's budget;
  PostToolUse (async) wakes the idle root.
- Frames run on `claude-code/claude-opus-5-5` (single completions through `claude -p`) when that provider is
  available, else on your default Ultron model; `--frame-model` changes it. Subagents are Claude Code processes by
  default (`claude -p` with their own `ultron mcp --child` server and kernel, reporting their verdict to the parent's
  host) on `claude-opus-5-5`, which `--child-model` changes. `--children ultron` runs them as Ultron lanes on your
  default model instead.
- What the external root cannot serve (`schedule.every`/`schedule.at`, `keep=True` sub-agents, `rlm.ask`) is refused
  with a clear error.
- Per-turn budgets count frames and subagents; the root's own usage is Claude Code's.

**Fair use.** Everything runs on your Claude Code login. Each frame and each subagent is its own Claude Code request
or process and counts against your plan's limits. By default all of them run Opus 5.5, so move the high-volume work
when a task fans out: `--frame-model claude-code/haiku` (or any `provider/model`) for frames, and
`--child-model sonnet` for subagents. The other defaults are conservative: subagents may not delegate further unless
given `depth=`, a turn admits at most 24 tasks, and the guide steers toward searching with code before any frame.

### Claude Code as a frame model

With the CLI installed and logged in, the `claude-code` provider runs model calls through it (`claude -p`) with no
API key, in any mode: `claude-code/haiku`, `claude-code/sonnet`, `claude-code/opus`, `claude-code/claude-opus-5-5`.
Tool-free calls serve inference frames, `/review` frames and judges. A lane with tools (the root agent under
`ultron --claude`, its subagents) runs on the `claude-subscription` provider instead.

```bash
ULTRON_RLM_FRAME_MODEL=claude-code/haiku ultron   # code-free rlm.infer/rlm.map frames go to the CLI
```

Or pick it in [`/settings → Models`](#models-settings--models) without restarting. Each call is isolated (no tools,
MCP servers, settings, hooks or `CLAUDE.md`, and its own system prompt) and uses your subscription's shared limits, so
at most four run at once (`ULTRON_CLAUDE_CODE_CONCURRENCY`). Details:
[providers](packages/coding-agent/docs/providers.md#claude-code-cli-claude-code).

## Optional services

- **Memory**: memorantado (`http://127.0.0.1:3789`) or Hindsight (`http://localhost:8888`; `ultron setup` can install
  it with Docker). [Memory](#5-memory-you-can-check) has how the backend is chosen, and
  [settings](packages/coding-agent/docs/settings.md#memory-backend-ultron) the keys.
- **Judge backends**: TypeSafe's Jev (`TYPESAFE_API_KEY`) and a self-hosted d1 server (`judge.d1.url`,
  `ULTRON_D1_KEY`). Without either, `judge.decide` uses `judge.fallbackModel` or abstains, and autoreview's blind
  check is skipped.
- **Neo4j** mirrors `knowledge.relations` and memorantado's memories when `graph.neo4j.url` is set (password from
  `NEO4J_PASSWORD` or auth.json `graph:neo4j`).

## Loki guardrails

[Loki](https://github.com/alfredosdpiii/loki) checks every file Ultron writes with deterministic rules and real
analyzers (hardcoded secrets, XSS and injection sinks, protected files, net-new Ruff/mypy/tsc/Oxlint/Clippy/Credo
findings). The Rust build carries its own port of the engine (`crates/loki`), and the bundled engine is 0.1.9. Without
any analyzer installed it still flags SQL built from strings, `eval` and pickle of request data, unsafe `yaml.load`,
open redirects and disabled TLS verification in Python; SQL built with `Sprintf` or `+`, `sh -c` with a variable and
`InsecureSkipVerify` in Go; and `danger_accept_invalid_certs` and SQL built with `format!` in Rust. It is on by default:

- `edit()` and `write()` are checked before the file changes; a finding raises `ValueError` and nothing is written.
  A check has no time limit unless `ULTRON_LOKI_TIMEOUT_MS` sets one; past it, the write goes through with a visible
  "Loki did not check this write" note.
- A `bash()` command that would write a source or configuration file in the project (a redirect, a here-document,
  `tee`, `sed -i`, `perl -i`, `cp`/`mv`/`install`, `dd of=`) is refused before it runs, with "bash command was not
  run" and Loki's reason, so a shell write cannot skip the check above; use `edit()`/`write()` instead. Writes into
  notes and data (`.md`, `.txt`, `.log`, `.patch`, ...), Git-ignored output, `/dev/null` and paths outside the project
  run as before, and commands that write no file never reach Loki.
- Files a cell changes any other way (`Path.write_text`, a program or build step) are checked after the cell, in the
  background; findings arrive with the next cell result. Only what the cell introduced is reported, in three kinds
  that are never mixed: new findings ("fix these"), advisory lines such as a function grown complex (an FYI, no
  change required), and checks that could not run (a missing analyzer, said once per session). Existing code in an
  edited file is not reported.
- A write into another repository (a sibling worktree) is checked against that repository's own `.loki/` policy.
- A newly imported Python module is checked against the project's interpreter: `ULTRON_LOKI_PROJECT_PYTHON`, else
  the `python` that `bash()` runs when it belongs to a virtual or conda environment, else a `.venv`. Without one the
  import is "not checked", never "unresolved".
- In a Git repository without `.loki/`, Ultron creates only `.loki/` (engine and default policy) and commits just
  that directory as "Add Loki guardrails" with your Git identity, leaving your other staged and unstaged changes
  alone. It never pushes and never skips hooks; if a hook or signing refuses the commit, or a merge or rebase is in
  progress, HEAD is detached or `CI` is set, `.loki/` stays uncommitted and the session says why. **On a fork, that
  commit rides along in your pull requests**; drop it or turn auto-commit off. A repository's own `.loki/` is used
  as committed and never modified.
- Turn it off with `ULTRON_LOKI=off` (or `ULTRON_LOKI=advise` to report without blocking), stop the auto-install or
  auto-commit with `ULTRON_LOKI_AUTOINIT=off` / `ULTRON_LOKI_AUTOCOMMIT=off`, or use `ultron setup` (the global
  `loki` setting, which also takes `ignoreRepos`).

## Safety

Model-written Python runs with your user's permissions, like Pi's `bash` tool, unless you turn on the shell sandbox.
Resource limits apply (the process-tree memory cap, CPU and wall-time budgets, per-request token, turn and call
limits), and Loki, secret masking and the profile write block catch some mistakes.

**The sandbox is off by default.** The `sandbox.mode` setting turns it on: `auto` uses the strongest mechanism that
works (bubblewrap, rootless `unshare` namespaces, or Docker when `sandbox.image` names an image) and runs unisolated
when none does; `required` refuses commands without one. Under it, shell commands and the REPL kernel see the project
and session directories writable, the home directory read-only with credentials hidden (`~/.ssh`, `~/.aws`,
`~/.config/gh`, `~/.ultron`, `~/.claude` and others), a private `/tmp`, and an environment cleared to an allowlist;
`sandbox.writable`, `readable`, `hidden`, `network` and `env` adjust it. Docker runs shell commands only: the kernel
then runs unsandboxed under `auto` and is refused under `required`. `ultron sandbox doctor` reports which mechanism a
session in the current directory gets and tries the escapes it must stop (a write outside the project, a read of a
hidden credential, the host's `/tmp`); it exits 1 when isolation does not hold. Without the sandbox, run Ultron in a
container or VM if you need a boundary; Pi's
[containerization guide](packages/coding-agent/docs/containerization.md) applies.

**Credentials.** `auth.json`, `mcp-auth.json`, `settings.json` and `trust.json` are written to a temporary file and
renamed into place, so an interrupted write never empties them, and `settings.json` and `trust.json` are created
readable only by you. An unreadable `trust.json` makes the project untrusted: its settings and resources are not
loaded. See [security](packages/coding-agent/docs/security.md).

## Where the ideas come from

Ultron combines ideas from several projects and papers:

- **Prime Intellect's RLM harness**: the REPL as the sole tool, shell and MCP as Python skills, output truncation.
- **NVIDIA's NOOA**: bounded previews instead of dumps, and agents as typed Python classes.
- **Autolith**: bounded inference over inputs the root never reads, with contracts, repair and budget trees.
- **LLM-as-Code**: context that collapses when work returns, and self-improvement committed as tested code.
- **Unreal Agent**: long work that never blocks the turn.
- **waku-agent**: memory behind a retrieval gate.
- **reflection-oss/mirror**: typed memory records, file-linked memory pointers, memory import, working-tree
  checkpoints and request traces.
- **[xmpuspus](https://github.com/xmpuspus)**: driving Claude Code headlessly through `claude -p`, the basis of
  `ultron claude`, `ultron --claude` and the `claude-code/*` provider.
- **Nous Research's Hermes plugin `claude-subscription-directsdk`**: the `claude-subscription` provider, ported.
- **[pstack](https://github.com/cursor/plugins/tree/main/pstack)** by Lauren Tan ([poteto](https://x.com/poteto)):
  the engineering skills bundled with Ultron (`/skill:rigor` and the skills it runs), ported to the REPL. See
  [packages/coding-agent/skills/pstack](packages/coding-agent/skills/pstack/README.md).
- **[HumanLayer](https://github.com/humanlayer/skills)**: the `show-me` skill, bundled as `/skill:diagram-it` with
  terminal-first views drawn from the code. See
  [packages/coding-agent/skills/humanlayer](packages/coding-agent/skills/humanlayer/README.md).
- **[Answer me with HTML](https://github.com/QingYunA/answer-me-with-html)** by QingYunA: bundled as
  `/skill:answer-me-with-html`, which answers a hard question with one HTML page (panels, diagrams, tables) rendered
  by its own `am` CLI. See [packages/coding-agent/skills/qingyuna](packages/coding-agent/skills/qingyuna/README.md).
- **[Anatomy](https://github.com/wheresryan22/anatomy)** by Ryan: bundled as `anatomy`, which draws an idea as a live
  isometric figure of a machine. See [packages/coding-agent/skills/wheresryan](packages/coding-agent/skills/wheresryan/README.md).

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
nice -n 19 cargo nextest run --workspace --locked   # cargo install cargo-nextest
scripts/rust-release.sh              # dist/rust-release/ultron-<version>-<target>.tar.gz; publishes nothing
specs/check.sh specs/spawn-collect   # check a Quint spec with TLC (needs Quint, Java 11+)
```

`scripts/rust-release.sh` builds a reproducible archive: the stripped binary, `install.sh`, README, LICENSE, NOTICE
and the third-party licenses, plus a `.sha256` file. `specs/` holds Quint specs of Ultron's own protocols
(`spawn-collect`, `schedule-fire`, `goal-driver`, `root-run`, `worker-ownership`); `specs/check.sh` checks each rule
in its own TLC run and compares the verdict with the spec's `expected` file, and the specs with a model of earlier
code reproduce the bugs that code had.

See [AGENTS.md](AGENTS.md) for the rules agents and contributors follow.

### Layout

- `crates/`: the Cargo workspace. `cli` is the `ultron` binary; `kernel` supervises the Python REPL; `host`,
  `kernel-apis`, `inference` and `delegation` implement the operations behind it; `session-core`, `daemon`,
  `server`, `client` and `protocol` run sessions; `ai` is the provider layer; `tui` and `interactive` the terminal
  UI; `loki` the write-time guardrails; `review` the `/review` and `ultron autoreview` engine; `sandbox` the shell
  sandbox; `judge` the judge seat; `memorantado` and `hindsight` the memory backends; `claude-root` the Claude Code
  modes.
- `packages/coding-agent/`: the assets the binary embeds or the tests read, kept at their original paths: the Python
  runtime (`src/ultron/rlm/*.py`, `secret-patterns.json`), the Loki engine, the bundled `skills/`, `docs/`,
  `CHANGELOG.md` and `package.json` (the version), the `--export` HTML template and the built-in themes.
- `scripts/rust-release.sh`, `scripts/rust-release/`: the release archive (binary, `install.sh`, licenses).
- `specs/`: Quint specs of Ultron's protocols and `check.sh`.
- `docs/rust-rewrite/`: the rewrite's baseline, decision log and feature ledger.

## Credits and license

Ultron is built on [Pi](https://github.com/badlogic/pi-mono) by Mario Zechner and contributors, and keeps Pi's
MIT license (see [LICENSE](LICENSE)). Third-party notices for what the binary embeds are in
[scripts/rust-release/NOTICE](scripts/rust-release/NOTICE); the release archive adds the Rust crates' licenses. It
bundles [Loki](https://github.com/alfredosdpiii/loki) (MIT, its license in
`packages/coding-agent/src/ultron/loki-engine/LICENSE`) and skills ported from
[pstack](https://github.com/cursor/plugins/tree/main/pstack) by Lauren Tan (MIT, its license in
`packages/coding-agent/skills/pstack/LICENSE`), HumanLayer's [`show-me`](https://github.com/humanlayer/skills) skill
as `diagram-it` (MIT, `packages/coding-agent/skills/humanlayer/LICENSE`), QingYunA's
[Answer me with HTML](https://github.com/QingYunA/answer-me-with-html) skill with its `am` CLI (MIT,
`packages/coding-agent/skills/qingyuna/LICENSE`) and Ryan's [Anatomy](https://github.com/wheresryan22/anatomy) skill
(MIT, `packages/coding-agent/skills/wheresryan/LICENSE`). Pi's documentation at [pi.dev](https://pi.dev) covers the
interface, providers and settings that Ultron shares.
