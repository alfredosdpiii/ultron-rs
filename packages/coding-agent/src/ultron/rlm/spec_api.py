"""The spec of a change: what the finished work must do, written before the work, and the pull request's
description built from it.

    import spec_api
    print(spec_api.write("Daily totals", "The page shows totals per day.",
                         ["A day with sales shows their sum", "A day without sales shows 0"]))
    await spec_api.check(bash, "pytest -q tests/test_totals.py")        # recorded with its exit code
    spec_api.mark("A1", "pass", "pytest -q tests/test_totals.py: 3 passed")
    spec_api.decide("Totals are per UTC day", "the ticket names no time zone", "the grouping key changes")
    path = spec_api.description()                                        # after the commit and the review
    # gh pr create --title "..." --body-file <path>

State lives beside the review's receipt, in `<git dir>/ultron-review/`: `spec.json` (the goal, the acceptance
items and the decisions of the branch), `checks.json` (commands with their exit code and the tree they ran on) and `description.md`.
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
from urllib.parse import quote

from review_api import SIZE_CEILING, Git, ReviewError, Runner, _receipt_path

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


def _aside(directory: Path, branch: str) -> Path:
    return directory / f"spec-{quote(branch, safe='')}.json"


def _branch_of(spec: Any) -> str | None:
    if not isinstance(spec, dict):
        return None
    branch = spec.get("branch")
    return branch if isinstance(branch, str) and branch else None


def _spec(git: Git, directory: Path) -> dict[str, Any] | None:
    """The spec of the checked-out branch, in `spec.json`. A stack switches branches inside one checkout, so the
    spec of another branch is set aside under its branch's name and brought back when that branch is checked out."""
    branch = git.value("rev-parse", "--abbrev-ref", "HEAD")
    if not branch:
        return None
    current = directory / "spec.json"
    spec = _read(current)
    other = _branch_of(spec)
    if other == branch:
        return spec
    if other:
        os.replace(current, _aside(directory, other))
    mine = _read(_aside(directory, branch))
    if _branch_of(mine) != branch:
        return None
    os.replace(_aside(directory, branch), current)
    return mine


def _render_acceptance_line(item: dict[str, Any]) -> str:
    evidence = f": {item['evidence']}" if item["evidence"] else ""
    return f"- {item['status']} {item['id']} {item['text']}{evidence}"


def _render(spec: dict[str, Any]) -> str:
    lines = [f"Spec: {spec['title']}", "", spec["goal"], ""]
    lines += [_render_acceptance_line(item) for item in spec["acceptance"]]
    if spec.get("decisions"):
        lines += ["", "Decisions:", *(_decision(item) for item in spec["decisions"])]
    return "\n".join(lines) + "\n"


def _decision(item: dict[str, Any]) -> str:
    return f"- {item['id']} {item['choice']}. Why: {item['reason']}. If overruled: {item['if_overruled']}."


def _clean_items(title: str, goal: str, acceptance: Any) -> list[str]:
    items = [text.strip() for text in acceptance] if isinstance(acceptance, list) else []
    if not title.strip() or not goal.strip() or not items or not all(items):
        raise ReviewError("a spec needs a title, a goal and at least one acceptance item, each a sentence a check "
                          "can show true or false")
    return items


def _merge_acceptance(items: list[str], old: dict[str, Any] | None) -> list[dict[str, Any]]:
    prior = old["acceptance"] if old else []
    kept = {item["text"]: item for item in prior}
    merged: list[dict[str, Any]] = []
    for number, text in enumerate(items, 1):
        prev = kept.get(text, {})
        merged.append({
            "id": f"A{number}",
            "text": text,
            "status": prev.get("status", "pending"),
            "evidence": prev.get("evidence", ""),
        })
    # What a review asked for stays until it is shown: rewording the spec does not answer it.
    for item in prior:
        if item.get("from") == "review" and item["text"] not in items:
            merged.append(dict(item, id=f"A{len(merged) + 1}"))
    return merged


def write(title: str, goal: str, acceptance: list[str], *, cwd: str | None = None, revise: str | None = None,
          runner: Runner | None = None) -> str:
    """Write down what the finished work must do, before the first edit: `goal` in a sentence or two, and
    `acceptance`, one sentence per behaviour a check can show true or false (failure paths included). Returns the
    spec as text, each item with its id (A1, A2, ...).

    A branch has one spec. Replacing it takes `revise`, what changed in the request; an item whose text stays
    keeps its result."""
    items = _clean_items(title, goal, acceptance)
    git, directory = _state(cwd, runner)
    old = _spec(git, directory)
    if old and not (revise or "").strip():
        raise ReviewError(f"this branch has a spec ({old['title']}): pass revise=\"<what changed in the request>\" "
                          "to replace it; an item whose text stays keeps its result")
    revisions = [*old["revisions"], revise.strip()] if old else []
    decisions = old.get("decisions", []) if old else []
    spec = {
        "branch": git.value("rev-parse", "--abbrev-ref", "HEAD"),
        "title": title.strip(),
        "goal": goal.strip(),
        "acceptance": _merge_acceptance(items, old),
        "revisions": revisions,
        "decisions": decisions,
    }
    _store(directory / "spec.json", json.dumps(spec, indent=1) + "\n")
    return _render(spec) + ("\nWhere the request leaves a choice open, pick, and record it: "
                            "spec_api.decide(choice, reason, if_overruled).\n")


def read(*, cwd: str | None = None, runner: Runner | None = None) -> str:
    """The branch's spec as text, or a line saying there is none."""
    spec = _spec(*_state(cwd, runner))
    return _render(spec) if spec else "No spec on this branch.\n"


def add_from_review(items: dict[str, str], *, cwd: str | None = None, runner: Runner | None = None) -> list[str]:
    """Acceptance items a review adds, as {marker: text}: an item whose text holds the marker is there already.
    Returns the new ids; none without a spec."""
    git, directory = _state(cwd, runner)
    spec = _spec(git, directory)
    added = []
    for marker, text in items.items() if spec else []:
        if not any(marker in item["text"] for item in spec["acceptance"]):
            added.append(f"A{len(spec['acceptance']) + 1}")
            spec["acceptance"].append({"id": added[-1], "text": text, "status": "pending", "evidence": "",
                                       "from": "review"})
    if added:
        _store(directory / "spec.json", json.dumps(spec, indent=1) + "\n")
    return added


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


def decide(choice: str, reason: str, if_overruled: str, *, cwd: str | None = None,
           runner: Runner | None = None) -> str:
    """Record a choice the request left open, instead of asking: what you chose, why, and what would have to
    change if the user overrules it. The description prints the decisions, so the reviewer of the pull request
    sees them. Recording the same choice again updates it."""
    git, directory = _state(cwd, runner)
    spec = _spec(git, directory)
    if spec is None:
        raise ReviewError("no spec on this branch: spec_api.write(title, goal, acceptance) first")
    fields = {"choice": choice, "reason": reason, "if_overruled": if_overruled}
    fields = {name: " ".join(str(text).split()).rstrip(".") for name, text in fields.items()}
    if not all(fields.values()):
        raise ReviewError("a decision needs the choice, the reason, and what changes if it is overruled")
    decisions = spec.setdefault("decisions", [])
    same = next((item for item in decisions if item["choice"] == fields["choice"]), None)
    if same:
        same.update(fields)
    else:
        decisions.append({"id": f"D{len(decisions) + 1}", **fields})
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


def _latest_checks(directory: Path, tree: str) -> dict[str, dict[str, Any]]:
    recorded = _read(directory / "checks.json")
    if not isinstance(recorded, list):
        return {}
    latest: dict[str, dict[str, Any]] = {}
    for record in recorded:
        if record.get("tree") == tree and not record.get("dirty"):
            latest[record["command"]] = record
    return latest


def _receipt_purposes(receipt: dict[str, Any]) -> list[dict[str, Any]]:
    raw = receipt.get("purposes") or []
    return [entry for entry in raw if isinstance(entry, dict) and entry.get("purpose")]


def _receipt_files(receipt: dict[str, Any]) -> list[str]:
    raw = receipt.get("files") or []
    return [path for path in raw if isinstance(path, str)]


def _stray_files(purposes: list[dict[str, Any]], files: list[str]) -> list[str]:
    if not purposes:
        return []
    named: set[str] = set()
    for purpose in purposes:
        for path in purpose.get("files") or []:
            named.add(path)
    return [path for path in files if path not in named]


def _spec_open_items(spec: dict[str, Any] | None) -> list[str]:
    if spec is None:
        return ["no spec says what this change must do (spec_api.write)"]
    return [f"{item['id']} is {item['status']}" for item in spec["acceptance"] if item["status"] != "pass"]


def _review_open_items(receipt: dict[str, Any], spec: dict[str, Any] | None,
                       latest: dict[str, dict[str, Any]]) -> list[str]:
    open_items: list[str] = []
    confirmed = receipt.get("confirmed")
    if confirmed != 0:
        open_items.append(f"{confirmed} confirmed finding(s) in the review")
    if receipt.get("incomplete") is not False:
        open_items.append("the review did not finish")
    open_items += _spec_open_items(spec)
    if not latest:
        open_items.append("no check ran on this tree (await spec_api.check(bash, \"<command>\"))")
    for command, record in latest.items():
        if record["exit_code"] != 0:
            open_items.append(f"`{command}`: {_exit(record)}")
    return open_items


def _scope_open_items(receipt: dict[str, Any], purposes: list[dict[str, Any]], files: list[str]) -> list[str]:
    open_items: list[str] = []
    if len(purposes) > 1:
        names = "; ".join(purpose["purpose"] for purpose in purposes)
        open_items.append(f"{len(purposes)} separate purposes ({names}): one pull request each")
    lines = receipt.get("lines")
    if isinstance(lines, int) and lines > SIZE_CEILING:
        open_items.append(f"{lines:,} changed lines, past {SIZE_CEILING:,}: split it by idea into a short stack "
                          "(review each branch against its parent branch), or waive with why it is one idea")
    stray = _stray_files(purposes, files)
    if stray:
        open_items.append(f"{len(stray)} file(s) under no purpose: {_names(stray)}")
    return open_items


def _render_purpose_line(purpose: dict[str, Any]) -> str:
    files = f" ({_names(purpose['files'])})" if purpose.get("files") else ""
    return f"- {purpose['purpose']}{files}"


def _append_tail_sections(out: list[str], spec: dict[str, Any] | None, latest: dict[str, dict[str, Any]]) -> None:
    if spec:
        out += ["## Acceptance", ""]
        out += [_render_acceptance_line(item) for item in spec["acceptance"]]
        out.append("")
    if latest:
        out += ["## Verification", ""]
        out += [f"- `{command}`: {_exit(record)} on this tree, last line `{record['last_line']}`"
                for command, record in latest.items()]
        out.append("")


def _render_description(tree: str, spec: dict[str, Any] | None, purposes: list[dict[str, Any]],
                        open_items: list[str], waived: str, latest: dict[str, dict[str, Any]]) -> str:
    out = [MARKER.format(tree=tree)]
    if spec:
        out += ["## Why", "", spec["goal"], ""]
    if purposes:
        out += ["## What changed", "", *(_render_purpose_line(purpose) for purpose in purposes), ""]
    if spec and spec.get("decisions"):
        out += ["## Decisions", "", *(_decision(item) for item in spec["decisions"]), ""]
    if open_items:
        out += ["## Scope", "", f"- Ships with open items ({waived}): {'; '.join(open_items)}", ""]
    _append_tail_sections(out, spec, latest)
    return "\n".join(out)


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
    latest = _latest_checks(directory, tree)
    purposes = _receipt_purposes(receipt)
    files = _receipt_files(receipt)
    open_items = _review_open_items(receipt, spec, latest) + _scope_open_items(receipt, purposes, files)
    waived = receipt.get("waived")
    waived = waived.strip() if isinstance(waived, str) else ""
    if open_items and not waived:
        raise ReviewError("the pull request is not ready: " + "; ".join(open_items) + ". Fix them, commit, review "
                          "and run this again. For ones that are wrong or belong in a follow-up, "
                          f"`review_api.waive(\"<reason>\", cwd={where})` records why, and the description prints it.")
    path = directory / "description.md"
    _store(path, _render_description(tree, spec, purposes, open_items, waived, latest))
    return str(path)
