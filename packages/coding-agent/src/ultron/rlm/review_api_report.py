"""Report rendering, ship receipts, waiving, and saving for `review_api`."""
from __future__ import annotations

import json
import os
import tempfile
import time
from pathlib import Path
from typing import Any, Callable

from review_prompts import Reviewer
from review_api_types import (
    FIND_SHARE,
    REPORT_FINDINGS,
    SEVERITIES,
    SEVERITY_TITLES,
    SIZE_CEILING,
    SIZE_TARGET,
    Chunk,
    FileDiff,
    Git,
    Options,
    Review,
    ReviewError,
    Runner,
    Scope,
    _clip,
    _receipt_path,
    default_branch,
)


def _quote(read_file: Callable[[str], list[str] | None], finding: dict[str, Any]) -> str:
    source = read_file(finding["file"])
    line = finding["line"]
    if not source or not 1 <= line <= len(source):
        return ""
    return f"    {line} | {_clip(source[line - 1].rstrip(), 200)}"


def _render_confirmed_detail(
    read_file: Callable[[str], list[str] | None],
    finding: dict[str, Any],
    shown: int,
) -> list[str]:
    where = f"{finding['file']}:{finding['line']}"
    if shown > REPORT_FINDINGS:
        return ["", f"- `{where}` {finding['claim']}"]
    by = ", ".join(finding["reviewers"])
    out = [
        "",
        f"**{shown}. `{where}`** {finding['claim']}",
        f"_{finding['category']}; raised by {by}; confidence {finding['confidence']:.1f}_",
    ]
    quote = _quote(read_file, finding)
    if quote:
        out += ["", quote, ""]
    if finding.get("evidence"):
        out.append(f"- Evidence: {finding['evidence']}")
    if finding.get("why"):
        out.append(f"- Why: {finding['why']}")
    if finding.get("suggested_fix"):
        out.append(f"- Fix: {finding['suggested_fix']}")
    return out


def _render_confirmed_section(
    confirmed: list[dict[str, Any]],
    read_file: Callable[[str], list[str] | None],
) -> list[str]:
    if not confirmed:
        return ["", "No confirmed findings."]
    out: list[str] = []
    shown = 0
    for severity in SEVERITIES:
        group = [finding for finding in confirmed if finding["severity"] == severity]
        if not group:
            continue
        out += ["", f"## {SEVERITY_TITLES[severity]} ({len(group)})"]
        for finding in group:
            shown += 1
            out += _render_confirmed_detail(read_file, finding, shown)
    return out


def _render_preexisting_section(preexisting: list[dict[str, Any]] | None) -> list[str]:
    if not preexisting:
        return []
    out = [
        "",
        f"## Pre-existing, not blocking ({len(preexisting)})",
        "",
        "Real issues in code this change did not alter; reported for visibility and never holding the pull request.",
        "",
    ]
    for finding in preexisting[:REPORT_FINDINGS]:
        ev = f" Evidence: {finding['evidence']}" if finding.get("evidence") else ""
        out.append(f"- `{finding['file']}:{finding['line']}` ({finding['severity']}) {finding['claim']}{ev}")
    return out


def _render_uncertain_section(uncertain: list[dict[str, Any]]) -> list[str]:
    if not uncertain:
        return []
    out = [
        "",
        f"## Uncertain, not confirmed ({len(uncertain)})",
        "",
        "None of these was ruled out. Where the verifier lacked code, read the code it names and decide: "
        "that is where a reviewer who reads past the diff finds what this review could not.",
        "",
    ]
    for finding in uncertain[:REPORT_FINDINGS]:
        note = finding.get("verification") or ""
        suffix = f" Verifier: {note}" if note else ""
        out.append(f"- `{finding['file']}:{finding['line']}` ({finding['severity']}) {finding['claim']}{suffix}")
    if len(uncertain) > REPORT_FINDINGS:
        out.append(f"- and {len(uncertain) - REPORT_FINDINGS} more in the saved report")
    return out


def _render_purposes_section(purposes: list[dict[str, Any]] | None) -> list[str]:
    if not purposes:
        return []
    out = ["", f"## Purposes of this change ({len(purposes)})", ""]
    out += [f"- {item['purpose']}" for item in purposes]
    if len(purposes) > 1:
        out += [
            "",
            "The pull request's description must name each one. A purpose it leaves out goes in its own pull request.",
        ]
    return out


def _render_summary_section(
    confirmed: list[dict[str, Any]],
    uncertain: list[dict[str, Any]],
    rejected: list[dict[str, Any]],
    stats: dict[str, Any],
    path: str | None,
    post_line: str | None,
) -> list[str]:
    out = [
        "",
        "## Summary",
        "",
        f"- Findings: {stats.get('raised', 0)} raised, {stats.get('merged', 0)} after merging duplicates, "
        f"{len(confirmed)} confirmed, {len(rejected)} rejected, {len(uncertain)} uncertain.",
    ]
    tokens = stats.get("tokens", 0)
    cost = (
        f"- Cost: {stats.get('frames', 0)} model calls for {stats.get('find_frames', 0)} finder and "
        f"{stats.get('verify_frames', 0)} verifier frames (re-asks included), {tokens:,} tokens of a "
        f"{stats.get('budget', 0):,}-token cap"
    )
    if stats.get("subagents"):
        cost += (
            f"; {stats['subagents']} sub-agents that read past the diff or re-checked a finding (their tokens "
            "count in the session usage)"
        )
    out.append(cost + ".")
    if path:
        out.append(f"- Full report: {path}")
    if post_line:
        out.append(f"- {post_line}")
    return out


def _render_gaps_section(static_ran: list[str] | None, not_checked: list[str]) -> list[str]:
    if static_ran:
        runtime_gap = (
            f"Runtime behavior: no project tests or full build were executed (isolated static checks ran: "
            f"{', '.join(static_ran)}); all other findings come from reading the diff, surrounding code and callers."
        )
    else:
        runtime_gap = (
            "Runtime behavior: nothing was executed (no tests, build or type check); findings come from reading the "
            "diff, the code around it and name-matched callers."
        )
    gaps = [
        runtime_gap,
        "Effects on code that does not mention the changed names (dynamic dispatch, reflection, other services).",
    ]
    return ["", "### What this review did not check", "", *(f"- {gap}" for gap in gaps + not_checked)]


def render_report(
    *,
    label: str,
    files: list[FileDiff],
    reviewers: list[Reviewer],
    confirmed: list[dict[str, Any]],
    uncertain: list[dict[str, Any]],
    rejected: list[dict[str, Any]],
    stats: dict[str, Any],
    not_checked: list[str],
    read_file: Callable[[str], list[str] | None],
    path: str | None = None,
    post_line: str | None = None,
    purposes: list[dict[str, Any]] | None = None,
    lines: int = 0,
    preexisting: list[dict[str, Any]] | None = None,
    static_ran: list[str] | None = None,
) -> str:
    """Markdown report: confirmed findings by severity, uncertain ones apart, then counts, cost and gaps.
    Rejected findings contribute only to the counts."""
    added = sum(item.added for item in files)
    removed = sum(item.removed for item in files)
    out = [
        f"# Code review: {label}",
        "",
        f"{len(files)} files, +{added} -{removed} lines. Reviewers: {', '.join(r.title for r in reviewers)}.",
    ]
    if lines > SIZE_TARGET:
        out += [
            "",
            f"Size: {lines:,} changed lines in the reviewed files. A pull request under {SIZE_TARGET} is "
            f"usually approved in one pass; past {SIZE_CEILING:,} the description is held. Split by idea "
            "into a short stack, and review each branch against its parent branch.",
        ]
    out += _render_confirmed_section(confirmed, read_file)
    out += _render_preexisting_section(preexisting)
    out += _render_uncertain_section(uncertain)
    out += _render_purposes_section(purposes)
    out += _render_summary_section(confirmed, uncertain, rejected, stats, path, post_line)
    out += _render_gaps_section(static_ran, not_checked)
    return "\n".join(out) + "\n"


def _validate_receipt_target(git: Git, scope: Scope, options: Options) -> tuple[str | None, Path | None, str | None]:
    if not scope.whole or scope.pr is not None or options.only is not None:
        return None, None, None
    tree = git.value("rev-parse", "HEAD^{tree}")
    path = _receipt_path(git)
    if not tree or path is None:
        return None, None, None
    code, dirty, _ = git.call("status", "--porcelain", "--untracked-files=no")
    if code != 0 or dirty.strip():
        return None, None, "No ship receipt: tracked files have uncommitted changes. Commit, then review again."
    return tree, path, None


def _next_receipt_round(git: Git, path: Path, tree: str) -> tuple[str, int]:
    default = default_branch(git)
    branch = f"{git.value('branch', '--show-current') or 'HEAD'} {default and git.value('merge-base', default, 'HEAD')}"
    try:
        previous = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        previous = None
    previous = previous if isinstance(previous, dict) else {}
    rounds = previous.get("round", 1) if previous.get("branch") == branch else 0
    round_ = rounds if previous.get("tree") == tree and rounds else rounds + 1
    return branch, round_


def _save_receipt_json(path: Path, receipt: dict[str, Any]) -> str | None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps(receipt, indent=1) + "\n", encoding="utf-8")
        os.replace(temporary, path)
        return None
    except OSError as error:
        return f"No ship receipt: {error}"


def _write_receipt(
    git: Git,
    scope: Scope,
    options: Options,
    confirmed: int,
    incomplete: bool,
    purposes: list[dict[str, Any]] | None = None,
    minor: int = 0,
    lines: int = 0,
) -> str | None:
    """Record that the committed tree was reviewed, for the gate on `gh pr create` (the `selfReview` setting,
    session-core situational_skills.rs). Returns the line the report shows, or None when no receipt applies.

    A receipt names a tree, so it is written only for a review of the whole local change with every reviewer and
    nothing uncommitted: uncommitted edits are in the review but not in the tree a pull request would carry.

    `confirmed` counts blockers and majors, which hold the pull request. `minor` counts the rest: reported, and
    never a reason for another round, since every fix of a small finding brings new small findings.

    `round` counts the reviews of this branch since its merge base: 1 for the first, one more for each review of
    another tree on the same branch. From round 2 the gate stops asking for fixes and wants the rest waived."""
    tree, path, note = _validate_receipt_target(git, scope, options)
    if tree is None or path is None:
        return note
    branch, round_ = _next_receipt_round(git, path, tree)
    # ponytail: a narrow base ref (HEAD~1) is taken at its word and recorded in `scope`; compare it with the pull
    # request's base if that gets used to review less than the pull request holds.
    receipt = {
        "tree": tree,
        "branch": branch,
        "round": round_,
        "confirmed": confirmed,
        "minor": minor,
        "incomplete": incomplete,
        "waived": None,
        "scope": scope.label,
        "files": [item.path for item in scope.files],
        "purposes": purposes or [],
        "lines": lines,
        "reviewed_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }
    err = _save_receipt_json(path, receipt)
    if err is not None:
        return err
    small = (
        f" The {minor} minor finding(s) and nits do not hold the pull request: fix the cheap ones once, list "
        "the rest in the description, and do not review again for them."
    ) if minor else ""
    return f"Ship receipt written for tree {tree[:8]}.{small}"


def waive(reason: str, *, cwd: str | None = None, runner: Runner | None = None) -> str:
    """Record why the reviewed tree ships with its confirmed findings or an unfinished review (they are wrong, or
    belong in a follow-up). The reason belongs in the pull request's description too."""
    if not reason.strip():
        raise ReviewError("waive needs the reason the findings may ship")
    git = Git(cwd or os.getcwd(), runner)
    path = _receipt_path(git)
    tree = git.value("rev-parse", "HEAD^{tree}")
    try:
        receipt = json.loads(path.read_text(encoding="utf-8")) if path else None
    except (OSError, ValueError):
        receipt = None
    if not receipt or receipt.get("tree") != tree:
        raise ReviewError("no review of the committed tree to waive: run the review first")
    receipt["waived"] = reason.strip()
    path.write_text(json.dumps(receipt, indent=1) + "\n", encoding="utf-8")
    return f"Waived for tree {tree[:8]}: {receipt['waived']}"


def _save(report: str, git: Git | None) -> str | None:
    stamp = time.strftime("%Y%m%d-%H%M%S")
    directory = None
    if git is not None:
        git_dir = git.value("rev-parse", "--absolute-git-dir")
        if git_dir:
            directory = Path(git_dir, "ultron-review")
    if directory is None:
        directory = Path(tempfile.gettempdir(), "ultron-review")
    saved: str | None = None
    try:
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / f"review-{stamp}.md"
        path.write_text(report, encoding="utf-8")
        saved = str(path)
    except OSError:
        saved = None
    return saved


def _plan_report(
    label: str,
    chunks: list[Chunk],
    plan: dict[str, Any],
    skipped: list[tuple[str, str]],
    options: Options,
) -> str:
    out = [
        f"# Review plan: {label}",
        "",
        f"{len(chunks)} chunks; {len(plan['frames'])} finder frames, about "
        f"{plan['estimate']:,} tokens of the {int(options.budget_tokens * FIND_SHARE):,}-token find budget.",
        "",
    ]
    for chunk in chunks:
        keys = [reviewer.key for reviewer, item in plan["frames"] if item is chunk]
        part = f" part {chunk.part}/{chunk.parts}" if chunk.parts > 1 else ""
        out.append(f"- {chunk.path}{part} ({chunk.kind}, {len(chunk.text):,} chars): {', '.join(keys) or 'none'}")
    for path, reason in skipped:
        out.append(f"- skipped {path}: {reason}")
    if plan["dropped"]:
        out.append(f"- over budget: {len(plan['dropped'])} frames would not run")
    return "\n".join(out) + "\n"


def _finish(report_args: dict[str, Any], git: Git | None, pr: int | None, post_requested: bool) -> Review:
    """Save the report and build the Review. The saved file is the comment `post` sends, so it names neither its
    local path nor the post status; the report shown in the session has both."""
    post_line = None
    if post_requested:
        if pr is None:
            post_line = "Not posted: --post needs a pull request (pass its number)."
        else:
            post_line = (
                f"Post pending: nothing has been posted to PR #{pr}. It is posted only after the "
                "user explicitly confirms."
            )
    path = _save(render_report(**report_args), git)
    report = render_report(**report_args, path=path, post_line=post_line)
    return Review(
        report,
        path,
        report_args["label"],
        report_args["confirmed"],
        report_args["uncertain"],
        report_args["rejected"],
        report_args["stats"],
        pr,
        post_requested and pr is not None,
    )


__all__ = [name for name in globals() if not name.startswith("__")]
