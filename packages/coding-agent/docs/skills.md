# Skills

Skills give Pi specialized instructions and supporting files for a particular kind of work. Pi advertises each available skill by name and description, then loads its full instructions only when the task calls for them.

Use a skill when a workflow needs more context than a prompt template but does not need a new executable integration point. Skills can bundle scripts, references, and assets alongside their instructions.

Pi implements the [Agent Skills specification](https://agentskills.io/specification). Most invalid fields produce warnings rather than stopping startup.

## Create a skill

A skill is a directory containing `SKILL.md`:

```text
pdf-tools/
├── SKILL.md
├── scripts/
│   └── extract.sh
├── references/
│   └── formats.md
└── assets/
    └── template.json
```

Start `SKILL.md` with frontmatter followed by direct instructions:

```markdown
---
name: pdf-tools
description: Extract text and tables from PDF files. Use when reading, converting, or inspecting PDFs.
---

# PDF tools

Read `references/formats.md` before converting a document. Run scripts relative to this skill directory.
```

The description determines when the model considers loading the skill. State both what the skill does and when it applies. Avoid descriptions such as “Helps with PDFs,” which do not provide enough routing information.

Use relative paths from the skill directory when referring to bundled files. Pi tells the model where the skill lives so it can resolve those paths.

## Understand how skills load

At startup, Pi scans configured skill locations and adds each skill’s name, description, and path to the system prompt. It does not add the full instructions.

When a task matches, the model reads `SKILL.md` and follows its instructions. This keeps detailed guidance out of context until it is needed. A model might fail to load a relevant skill, so use `/skill:name` when you need to force it.

Arguments after `/skill:name` are appended to the loaded instructions as a user request:

```text
/skill:pdf-tools extract report.pdf
```

To have a skill apply to every task without reading it or invoking it, name it in the `alwaysSkills` [setting](settings.md) and turn on engineering mode with `/engineering true` (or `"engineering": true`):

```json
{ "alwaysSkills": ["ponytail"], "engineering": true }
```

While engineering mode is on, its full text (frontmatter removed) goes in the system prompt, `ultron claude` and Claude Code subagents included, and it leaves the list of skills to read when a task matches. `/engineering false` turns the mode off: the skill is listed like any other from the next message. `/engineering` alone shows the mode. The mode is saved, so it holds across sessions; a running `ultron claude` session and its Claude Code subagents read it when they start. Always-on skills share a 16,000-character limit; a skill past it, or a name with no loaded skill, is a load warning and the skill stays listed as usual. `--no-skills` turns them off.

<a id="situational-skills"></a>

In engineering mode, a skill can also come to the model when its situation does. When a cell writes a file matching a rule's paths with `edit()` or `write()`, the cell's result ends with that skill's text, once per session and subagent (a subagent in its own worktree gets none), whatever the model would have read on its own. A file changed by other means (a script, a build, a checkout, another session working in the same directory) brings no skill, because the host cannot tell whose change it is. The rules are the `situationalSkills` setting:

```json
{
  "situationalSkills": [
    { "skill": "principle-explain-the-number", "paths": ["bench/**"], "situation": "You changed a benchmark" }
  ]
}
```

Without the setting, a test file brings `principle-test-behavior-not-implementation`, a Markdown or `docs/` file `principle-minimize-reader-load`, a `.ts`/`.tsx` file `typescript-best-practices`, a migration, SQL, Dockerfile, systemd unit or CI workflow `principle-make-operations-idempotent`, a benchmark `principle-explain-the-number`, an API, route, handler or CLI source file `principle-boundary-discipline`, and Rust, Go, Java, Kotlin, Swift, C# or Scala `principle-type-system-discipline`. A file in a sibling worktree outside the session directory matches by its path in its own repository. `false` or `[]` turns it off. A rule whose skill is always on is skipped, one skill comes per cell, and a skill over 4,000 characters is named with its file instead of shown.

<a id="self-review"></a>

In engineering mode, a pull request also needs a review of what it carries (the `selfReview` setting, `require` by default). `/review` with no paths and every reviewer, run on a clean tree, writes a receipt to `<git dir>/ultron-review/receipt.json` that names the committed tree. `gh pr create` is refused until the receipt names the tree at `HEAD`, the review finished, and it confirmed no blocker or major finding. Minor findings and nits are reported and do not hold the pull request: fix the cheap ones once and list the rest in the description, since each fix of a small finding brings new small findings. A commit after the review needs a new review. The review reads past the diff in two ways: for each endpoint the change names, a finder gets the lines of the handler that say what it answers and the other code that calls it; and a verifier that cannot decide names the code it lacks and is asked again with that code, up to three times. A review of the whole local change also starts sub-agents, on the sub-agent model of `/settings -> Models` (a cheap one fits): one per changed code or test file, 12 at most, follows the file into what it calls, what it replaces and what its test doubles stand in for, and its findings go to the same verifier; one more re-checks each blocker or major the verifier left undecided. They read only and run no tests; `--deep=0` turns them off. With a spec on the branch, the review adds one pending acceptance item per endpoint the change calls (5 at most): a run against the real handler, which no reading shows, so the description is refused until each is marked or waived. What stays under "Uncertain" was not ruled out: read the code it names. The report also lists the commands the repository's GitHub workflows run, none of which the review runs. A full `/review` also recalls up to 8 memories that start with `Review lesson:` (the project's, then yours) and gives them a finder of their own, so a mistake a reviewer flagged before is looked for in the next change; keep one with `memory.propose("Review lesson: ...", evidence, scope="global")`. Dreaming writes them too: each night it reads what reviewers said on your pull requests of the last 30 days (through `gh`, read only) and keeps a mistake raised on two or more of them, worded without names. Before it keeps one, the judge seat (`judge.decide`: Jev or d1, then `judge.escalateModel`) says whether a kept lesson already covers it; a duplicate, or a verdict the judge did not settle, is not kept. The report also lists the separate purposes of the change with the files that serve each; the description is built from them (see below). For findings that are wrong or belong in a follow-up, `review_api.waive("<reason>")` records the reason and lets the pull request through; the description prints it. Commits and pushes are refused once per session as a reminder, and then go through. Set `"selfReview": "advise"` for the reminder alone, or `"off"` for neither.

<a id="spec"></a>

Under `require`, a pull request also needs a description built from the change:

1. Before the first edit, `spec_api.write(title, goal, acceptance)` records what the finished work must do, one sentence per behaviour a check can show true or false. The playbook that opens a request asks for it.
2. `await spec_api.check(bash, command)` runs a check and records its exit code for the tree it ran on. A command that hides its exit code behind a pipe or `|| true` is refused.
3. `spec_api.mark(id, status, evidence)` records how each item ended: `pass`, `fail` or `blocked`.
4. `spec_api.decide(choice, reason, if_overruled)` records a choice the request left open, instead of asking. The description prints each one under `## Decisions`.
5. A full `/review` holds the diff against the items with a finder of their own.
6. `spec_api.description()` writes `<git dir>/ultron-review/description.md` for the committed tree. `gh pr create` is refused until `--body-file` names a description written for the tree at `HEAD`.

`description` refuses while something is open and names it: no spec, an item that did not pass, no passing check on this tree, more than one purpose, more than 1,000 changed lines (the report notes it from 400; generated files and lockfiles are not counted), a changed file under no purpose, or a confirmed finding. Nobody is asked to approve a step. What is open is fixed, or shipped with the reason `review_api.waive` recorded, which the description prints under `## Scope`. A push of a tree the description was not written for is refused once, with how to renew it. `/skill:mission` runs work too big for one session the same way: a baseline run of the test command in a fresh worktree before any feature is built, workers in worktrees held to host-run checks, and a validator per milestone that did not build what it checks.

Set `disable-model-invocation: true` in frontmatter when a skill should be available only through its explicit command. The `enableSkillCommands` [setting](settings.md) controls whether skill commands appear in interactive command discovery; manually entered `/skill:name` commands still work.

<a id="choose-where-it-loads"></a>

## Add it to Pi

Place the skill in your user or project skills directory. Directories containing `SKILL.md` are discovered recursively.

Pi also supports the Agent Skills locations `~/.agents/skills/` and `.agents/skills/`. Project `.agents/skills/` directories are discovered from the working directory through its ancestors, stopping at the repository root when one exists.

Pi accepts some standalone Markdown skills, but a directory containing `SKILL.md` is the portable form and should be preferred. See [Settings](settings.md#resources) and [Pi Packages](packages.md) for additional locations.

Project skills can instruct the model to run scripts or modify files. Review unfamiliar skills and their supporting files before granting project trust.

## Write portable frontmatter

The Agent Skills specification defines these fields:

| Field | Purpose |
|---|---|
| `name` | Command and display name |
| `description` | Routing description shown to the model |
| `license` | License name or bundled license file |
| `compatibility` | Environment requirements |
| `metadata` | Additional key-value metadata |
| `allowed-tools` | Experimental pre-approved tool list |
| `disable-model-invocation` | Hide the skill from automatic model selection |

Names use lowercase letters, numbers, and hyphens, with no leading, trailing, or consecutive hyphens. They can contain at most 64 characters; descriptions can contain at most 1024.

Pi neither requires nor warns when the declared name differs from the parent directory. Other Agent Skills implementations may enforce that requirement, so matching names remain the portable choice.

Malformed `SKILL.md` files and declared skills without descriptions are not loaded. Name collisions keep the first discovered skill and produce a warning.

## Validate and share a skill

Run Pi from a location where the skill is discoverable, then inspect the startup diagnostics and `/skill:name` command. Run `/reload` after editing a skill during an active session.

Use a [Pi package](packages.md) to distribute one or more skills through npm or git. Keep environment setup inside the skill and declare any required runtime dependencies in the package.

For examples, see the [Anthropic skills collection](https://github.com/anthropics/skills) and [Pi skills collection](https://github.com/badlogic/pi-skills).
