"""The spec of a change: what the finished work must do, written before the work, and the pull request's
description built from it.

    import spec_api
    print(spec_api.write("Daily totals", "The page shows totals per day.",
                         ["A day with sales shows their sum", "A day without sales shows 0"]))
    await spec_api.check(bash, "pytest -q tests/test_totals.py")        # recorded with its exit code
    spec_api.mark("A1", "pass", "pytest -q tests/test_totals.py: 3 passed")
    path = spec_api.description()                                        # after the commit and the review
    # gh pr create --title "..." --body-file <path>

State lives beside the review's receipt, in `<git dir>/ultron-review/`: `spec.json` (the goal and the acceptance
items of the branch), `checks.json` (commands with their exit code and the tree they ran on) and `description.md`.
`description` is the one place that decides whether a pull request is ready: the committed tree is reviewed, every
acceptance item passed, a check passed on that tree, the change has one purpose and every changed file serves it.
What is still open is either fixed or shipped with the reason `review_api.waive` recorded, which the description
prints. Nobody is asked to approve anything; the gate on `gh pr create` (session-core situational_skills.rs) takes
only a description written for the committed tree."""

from __future__ import annotations

import json
import os
import re
import time
from pathlib import Path
from typing import Any

from review_api import Git, ReviewError, Runner, _receipt_path

STATUSES = ("pending", "pass", "fail", "blocked")
#: The first line of a description: the tree it was written for. A new commit is a new tree.
MARKER = "<!-- ultron-description tree={tree} -->"
CHECKS_KEPT = 50
_QUOTED = re.compile(r"'[^']*'|\"[^\"]*\"")


def _state(cwd: str | None, runner: Runner | None) -> tuple[Git, Path]:
    git = Git(cwd or os.getcwd(), runner)
    receipt = _receipt_path(git)
    if receipt is None:
        raise ReviewError(f"{git.cwd} is not in a git repository")
    return git, receipt.parent


def _read(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _store(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(text, encoding="utf-8")
    os.replace(temporary, path)


def _spec(git: Git, directory: Path) -> dict[str, Any] | None:
    # ponytail: one spec per checkout, valid on the branch it was written on; key the file by branch if switching
    # branches inside one checkout loses specs people wanted.
    spec = _read(directory / "spec.json")
    if not isinstance(spec, dict) or spec.get("branch") != git.value("rev-parse", "--abbrev-ref", "HEAD"):
        return None
    return spec


def _render(spec: dict[str, Any]) -> str:
    lines = [f"Spec: {spec['title']}", "", spec["goal"], ""]
    for item in spec["acceptance"]:
        evidence = f": {item['evidence']}" if item["evidence"] else ""
        lines.append(f"- {item['status']} {item['id']} {item['text']}{evidence}")
    return "\n".join(lines) + "\n"


def write(title: str, goal: str, acceptance: list[str], *, cwd: str | None = None, revise: str | None = None,
          runner: Runner | None = None) -> str:
    """Write down what the finished work must do, before the first edit: `goal` in a sentence or two, and
    `acceptance`, one sentence per behaviour a check can show true or false (failure paths included). Returns the
    spec as text, each item with its id (A1, A2, ...).

    A branch has one spec. Replacing it takes `revise`, what changed in the request; an item whose text stays
    keeps its result."""
    items = [text.strip() for text in acceptance] if isinstance(acceptance, list) else []
    if not title.strip() or not goal.strip() or not items or not all(items):
        raise ReviewError("a spec needs a title, a goal and at least one acceptance item, each a sentence a check "
                          "can show true or false")
    git, directory = _state(cwd, runner)
    old = _spec(git, directory)
    if old and not (revise or "").strip():
        raise ReviewError(f"this branch has a spec ({old['title']}): pass revise=\"<what changed in the request>\" "
                          "to replace it; an item whose text stays keeps its result")
    kept = {item["text"]: item for item in old["acceptance"]} if old else {}
    spec = {
        "branch": git.value("rev-parse", "--abbrev-ref", "HEAD"),
        "title": title.strip(),
        "goal": goal.strip(),
        "acceptance": [{"id": f"A{number}", "text": text, "status": kept.get(text, {}).get("status", "pending"),
                        "evidence": kept.get(text, {}).get("evidence", "")}
                       for number, text in enumerate(items, 1)],
        "revisions": [*(old["revisions"] if old else []), *([revise.strip()] if old else [])],
    }
    _store(directory / "spec.json", json.dumps(spec, indent=1) + "\n")
    return _render(spec)


def read(*, cwd: str | None = None, runner: Runner | None = None) -> str:
    """The branch's spec as text, or a line saying there is none."""
    spec = _spec(*_state(cwd, runner))
    return _render(spec) if spec else "No spec on this branch.\n"


def mark(item_id: str, status: str, evidence: str, *, cwd: str | None = None, runner: Runner | None = None) -> str:
    """Record how an acceptance item ended: "pass", "fail" or "blocked" (it could not be checked here; that is
    not a pass), with its evidence: the command and what it showed, or the file and lines."""
    git, directory = _state(cwd, runner)
    spec = _spec(git, directory)
    if spec is None:
        raise ReviewError("no spec on this branch: spec_api.write(title, goal, acceptance) first")
    if status not in STATUSES:
        raise ReviewError(f"status is one of {', '.join(STATUSES)}, not {status!r}")
    item = next((item for item in spec["acceptance"] if item["id"] == item_id), None)
    if item is None:
        raise ReviewError(f"no acceptance item {item_id}; the spec has "
                          f"{', '.join(item['id'] for item in spec['acceptance'])}")
    if status != "pending" and not evidence.strip():
        raise ReviewError(f"{item_id}: {status} needs its evidence: the command and what it showed, or the file "
                          "and lines")
    item["status"], item["evidence"] = status, " ".join(evidence.split())
    _store(directory / "spec.json", json.dumps(spec, indent=1) + "\n")
    return _render(spec)


def _hides_exit_code(command: str) -> str | None:
    """What in `command` makes its exit code someone else's: a failing test piped through `tail` exits 0."""
    bare = _QUOTED.sub("''", command)
    if re.search(r"\|\|\s*(true|:)(\s|;|$)", bare):
        return "`|| true`"
    if "pipefail" not in bare and re.search(r"(?<!\|)\|(?!\|)", bare):
        return "a pipe"
    return None


async def check(bash: Any, command: str, *, cwd: str | None = None, runner: Runner | None = None) -> dict[str, Any]:
    """Run `command` with the session's `bash` and record its exit code, its last line of output and the tree it
    ran on (`cwd` names the repository; the command runs where `bash` runs). A run on uncommitted tracked changes
    is recorded as dirty: it says nothing about the tree a pull request carries."""
    hidden = _hides_exit_code(command)
    if hidden:
        raise ReviewError(f"`{command}` hides its exit code behind {hidden}: run the check itself, and narrow it "
                          "to the tests you need if its output is long")
    git, directory = _state(cwd, runner)
    ran = await bash(command, yield_after=None)
    output = str(getattr(ran, "output", ran))
    code, dirty, _ = git.call("status", "--porcelain", "--untracked-files=no")
    record = {
        "command": command,
        "exit_code": None if getattr(ran, "timed_out", False) else getattr(ran, "exit_code", None),
        "tree": git.value("rev-parse", "HEAD^{tree}"),
        "dirty": code != 0 or bool(dirty.strip()),
        "last_line": next((line.strip()[:200] for line in reversed(output.splitlines()) if line.strip()), ""),
        "at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }
    checks = _read(directory / "checks.json")
    checks = [*(checks if isinstance(checks, list) else []), record][-CHECKS_KEPT:]
    _store(directory / "checks.json", json.dumps(checks, indent=1) + "\n")
    return record


def _exit(record: dict[str, Any]) -> str:
    return "did not finish" if record["exit_code"] is None else f"exit {record['exit_code']}"


def _names(paths: list[str], limit: int = 6) -> str:
    more = f" and {len(paths) - limit} more" if len(paths) > limit else ""
    return ", ".join(f"`{path}`" for path in paths[:limit]) + more


def description(*, cwd: str | None = None, runner: Runner | None = None) -> str:
    """Write the pull request's description for the committed tree and return its path, for
    `gh pr create --body-file <path>`. Raises ReviewError naming what is still open; see the module's text. You may
    add sections to the file below its first line."""
    git, directory = _state(cwd, runner)
    where = repr(git.cwd)
    tree = git.value("rev-parse", "HEAD^{tree}")
    receipt = _read(directory / "receipt.json")
    if not tree or not isinstance(receipt, dict) or receipt.get("tree") != tree:
        raise ReviewError(f"the committed tree {(tree or 'none')[:8]} has not been reviewed: commit the change, run "
                          f"`review = await review_api.run(rlm, \"\", cwd={where})`, then this again")
    spec = _spec(git, directory)
    recorded = _read(directory / "checks.json")
    latest: dict[str, dict[str, Any]] = {}
    for record in recorded if isinstance(recorded, list) else []:
        if record.get("tree") == tree and not record.get("dirty"):
            latest[record["command"]] = record
    purposes = [p for p in receipt.get("purposes") or [] if isinstance(p, dict) and p.get("purpose")]
    files = [path for path in receipt.get("files") or [] if isinstance(path, str)]

    open_items: list[str] = []
    confirmed = receipt.get("confirmed")
    if confirmed != 0:
        open_items.append(f"{confirmed} confirmed finding(s) in the review")
    if receipt.get("incomplete") is not False:
        open_items.append("the review did not finish")
    if spec is None:
        open_items.append("no spec says what this change must do (spec_api.write)")
    else:
        open_items += [f"{item['id']} is {item['status']}" for item in spec["acceptance"] if item["status"] != "pass"]
    if not latest:
        open_items.append("no check ran on this tree (await spec_api.check(bash, \"<command>\"))")
    open_items += [f"`{command}`: {_exit(record)}" for command, record in latest.items() if record["exit_code"] != 0]
    if len(purposes) > 1:
        open_items.append(f"{len(purposes)} separate purposes ({'; '.join(p['purpose'] for p in purposes)}): "
                          "one pull request each")
    named = {path for p in purposes for path in p.get("files") or []}
    stray = [path for path in files if path not in named] if purposes else []
    if stray:
        open_items.append(f"{len(stray)} file(s) under no purpose: {_names(stray)}")

    waived = receipt.get("waived")
    waived = waived.strip() if isinstance(waived, str) else ""
    if open_items and not waived:
        raise ReviewError("the pull request is not ready: " + "; ".join(open_items) + ". Fix them, commit, review "
                          "and run this again. For ones that are wrong or belong in a follow-up, "
                          f"`review_api.waive(\"<reason>\", cwd={where})` records why, and the description prints it.")

    out = [MARKER.format(tree=tree)]
    if spec:
        out += ["## Why", "", spec["goal"], ""]
    if purposes:
        out += ["## What changed", ""]
        out += [f"- {p['purpose']}" + (f" ({_names(p['files'])})" if p.get("files") else "") for p in purposes]
        out.append("")
    if open_items:
        out += ["## Scope", "", f"- Ships with open items ({waived}): {'; '.join(open_items)}", ""]
    if spec:
        out += ["## Acceptance", ""]
        out += [f"- {item['status']} {item['id']} {item['text']}" + (f": {item['evidence']}" if item["evidence"] else "")
                for item in spec["acceptance"]]
        out.append("")
    if latest:
        out += ["## Verification", ""]
        out += [f"- `{command}`: {_exit(record)} on this tree, last line `{record['last_line']}`"
                for command, record in latest.items()]
        out.append("")
    path = directory / "description.md"
    _store(path, "\n".join(out))
    return str(path)
