"""Async pipeline stages, subagent coordinators, PR posting, and `run` stage helpers for `review_api`."""
from __future__ import annotations

import asyncio
import json
import os
import shutil
import time
from pathlib import Path
from typing import Any, Callable, Sequence

from infer_api import Budget, FrameError
from review_prompts import (
    DEEP_VERIFY_BRIEF,
    EXPLORE_BRIEF,
    REVIEWERS,
    SWEEP_TASK,
    VERIFIER_TASK,
    Reviewer,
    finder_task,
)
from review_api_diff import parse_args, plan_find, resolve_scope
from review_api_findings import (
    apply_verdicts,
    dedupe,
    needed_code,
    normalize_findings,
    related_code,
    similar_claims,
)
from review_api_inventories import (
    _acceptance_items,
    _knowledge_rules,
    _lessons,
    _purposes,
    _run_items,
    acceptance_reviewer,
    ci_commands,
    contract_inventory,
    contract_reviewer,
    conventions_inventory,
    conventions_reviewer,
    lessons_reviewer,
    out_of_diff_callers,
    recall_memorantado_items,
    reuse_inventory,
    reuse_reviewer,
)
from review_api_report import _finish, _write_receipt
from review_api_types import (
    AUTO_BUDGET_HEADROOM,
    AUTO_BUDGET_MAX,
    CHILD_POLL_S,
    CONTRACT_KEY,
    CONVENTIONS_KEY,
    DEDUPE_WINDOW,
    DEEP_MAX,
    DEEP_TIMEOUT_MS,
    EMPTY_TREE,
    EXPLORE_HUNK_CHARS,
    EXPLORE_KEY,
    EXPLORE_TIMEOUT_MS,
    EXPLORERS,
    FIND_SHARE,
    FRAME_TIMEOUT_MS,
    GH_TIMEOUT_S,
    MAX_FINDINGS_PER_FRAME,
    NEED_ROUNDS,
    NEEDS_CONTRACT,
    REUSE_KEY,
    SWEEP_DIFF_CHARS,
    SWEEP_KEY,
    USAGE,
    DEFAULT_BUDGET_TOKENS,
    FINDINGS_CONTRACT,
    Chunk,
    FileDiff,
    Git,
    Options,
    Review,
    ReviewError,
    Runner,
    Scope,
    _clip,
    _estimate_tokens,
    _failure,
    _rank,
    _rev_reader,
    _run_process,
    _spent,
    _text,
    file_kind,
    hunk_for,
    parse_json_object,
    render_hunk,
    skip_reason,
    source_window,
)


def _prepare_sandbox_target(git: Git, scope: Scope) -> tuple[Any, list[str], str] | None:
    test_paths = [item.path for item in scope.files if not skip_reason(item) and file_kind(item.path) == "test"]
    if not test_paths:
        return None
    try:
        import autoreview_deep as deep
    except ImportError:
        deep = None
    if deep is None:
        return None
    head = git.value("rev-parse", "HEAD")
    if not head:
        return None
    code, dirty, _ = git.call("status", "--porcelain", "--untracked-files=no")
    if code != 0 or dirty.strip():
        return None
    return deep, test_paths, head


def _observed_test_findings(observed: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "file": item["file"],
            "line": item.get("line") or 1,
            "severity": "major",
            "category": "tests",
            "claim": item["claim"],
            "why": item.get("why") or "The changed tests failed in the sandbox.",
            "suggested_fix": item.get("suggested_fix") or "Fix the failing test or code before opening the pull request.",
            "confidence": 1.0,
            "reviewers": ["sandbox:tests"],
            "evidence": item.get("evidence") or "sandboxed test run exited non-zero",
            "introduced": "this_change",
        }
        for item in observed
    ]


def _record_sandbox_note(started: dict[str, Any], test_paths: list[str], not_checked: list[str]) -> None:
    if started.get("note"):
        not_checked.append(started["note"][0].upper() + started["note"][1:] + ".")
    elif started.get("shown"):
        not_checked.append(f"Sandboxed tests passed: {', '.join(test_paths[:6])}.")


def _run_sandboxed_tests(git: Git, scope: Scope, not_checked: list[str]) -> list[dict[str, Any]]:
    """Run changed test files in a read-only sandbox (reusing local .venv/node_modules if present) on a whole local
    self-review. Returns host-confirmed major findings for any failing test file."""
    target = _prepare_sandbox_target(git, scope)
    if target is None:
        return []
    deep, test_paths, head = target
    started = None
    try:
        repo = deep.Repo(scope.root, head, git._runner)
        brief = deep.Brief(text="", lenses=[], tests=test_paths[:6])
        started = deep.start_tests(
            repo,
            scope.files,
            {"base": scope.base, "runs": 2, "timeout_s": 180, "checkout": scope.root},
            brief,
            root=scope.root,
            rev=head,
            clock=time.monotonic,
        )
        observed = list(started.get("observed") or [])
        if observed:
            return _observed_test_findings(observed)
        _record_sandbox_note(started, test_paths, not_checked)
    except Exception as error:  # noqa: BLE001 - sandboxed test run degrades cleanly
        not_checked.append(f"Sandboxed tests did not run ({_text(f'{type(error).__name__}: {error}', 160)}).")
    finally:
        if started and started.get("session") is not None:
            started["session"].close()
    return []


def _build_sweep_views(
    git: Git | None,
    scope: Scope,
    chunks: list[Chunk],
    merged: list[dict[str, Any]],
) -> list[str]:
    ordered = sorted(chunks, key=lambda chunk: file_kind(chunk.path) == "test")
    diff_text = "\n\n".join(chunk.text for chunk in ordered)[:SWEEP_DIFF_CHARS]
    already = "\n".join(
        f"- {finding['file']}:{finding['line']} ({finding['severity']}, {finding['category']}): {finding['claim']}"
        for finding in merged[:40]
    ) or "(none)"
    views = [
        f"Already-raised findings (do not repeat or reword these):\n{already}",
        f"Diff under review:\n{diff_text}",
    ]
    callers = out_of_diff_callers(git, scope) if git is not None else ""
    if callers:
        views.append(f"Out-of-diff callers of changed/added definitions:\n{callers}")
    return views


def _resolve_sweep_chunk(item: Any, by_path: dict[str, Chunk]) -> Chunk | None:
    if not isinstance(item, dict):
        return None
    named = str(item.get("file") or "").strip().strip("`\"'")
    for prefix in ("./", "a/", "b/"):
        if named.startswith(prefix) and named not in by_path:
            named = named[len(prefix):]
    return by_path.get(named)


def _normalize_sweep_results(raw: Any, scope: Scope, chunks: list[Chunk]) -> list[dict[str, Any]]:
    if not isinstance(raw, list):
        return []
    by_path: dict[str, Chunk] = {}
    for chunk in chunks:
        by_path.setdefault(chunk.path, chunk)
    reviewer = Reviewer(SWEEP_KEY, "Gap sweep", "", ())
    out: list[dict[str, Any]] = []
    for item in raw[:6]:
        chunk = _resolve_sweep_chunk(item, by_path)
        if chunk is None:
            continue
        source = scope.read_file(chunk.path)
        out += normalize_findings([item], reviewer, chunk, len(source) if source else None)
    return out


_FRAME_ERRORS = (OSError, RuntimeError, TimeoutError, ValueError, TypeError, KeyError, AttributeError, FrameError)


async def _gap_sweep(
    rlm: Any,
    scope: Scope,
    chunks: list[Chunk],
    merged: list[dict[str, Any]],
    model: str | None,
    budget_tokens: int,
    git: Git | None = None,
) -> tuple[list[dict[str, Any]], int, int]:
    """Phase 3 gap sweep: one fresh frame seeing the diff and already-merged findings, hunting only for second-tier
    misses not already listed. Returns (candidates, frames_used, tokens_used)."""
    if not chunks or budget_tokens < 4_000:
        return [], 0, 0
    views = _build_sweep_views(git, scope, chunks, merged)
    try:
        raw = await rlm.infer(
            SWEEP_TASK,
            views,
            contract=FINDINGS_CONTRACT,
            budget=Budget(tokens=min(budget_tokens, 40_000)),
            model=model,
            timeout_ms=FRAME_TIMEOUT_MS,
        )
    except _FRAME_ERRORS:
        return [], 0, 0
    return _normalize_sweep_results(raw, scope, chunks), 1, _estimate_tokens(SWEEP_TASK, *views, output=800)


async def _saved_review_model(rlm: Any) -> str | None:
    """`review.model` as the host reads it now (a /settings change applies to the next review); None when unset,
    so the frames take `rlm.frameModel` or the session's model from the host."""
    models = getattr(rlm, "_models", None)
    if not callable(models):
        return None
    review: dict[str, Any] = {}
    try:
        review = (await models()).get("review") or {}
    except (OSError, RuntimeError, TimeoutError, ValueError, TypeError, KeyError, AttributeError):
        review = {}
    model = review.get("model")
    return model if review.get("source") == "setting" and isinstance(model, str) else None


async def _run_find_stage(
    rlm: Any,
    scope: Scope,
    frames: list[tuple[Reviewer, Chunk]],
    *,
    find_budget: int,
    model: str | None,
    stats: dict[str, Any],
    not_checked: list[str],
) -> tuple[list[dict[str, Any]], dict[str, list[str]]]:
    from review_shared import find_frame_input

    raised: list[dict[str, Any]] = []
    failures: dict[str, list[str]] = {}
    if not frames:
        return raised, failures
    find_tasks: list[str] = []
    find_items: list[str | list[str]] = []
    for reviewer, chunk in frames:
        task, item = find_frame_input(
            reviewer,
            chunk.text,
            conventions_key=CONVENTIONS_KEY,
            reviewer_cls=Reviewer,
            finder_task=finder_task,
        )
        find_tasks.append(task)
        find_items.append(item)
    results = await rlm.map(
        find_tasks,
        find_items,
        contract=FINDINGS_CONTRACT,
        budget=Budget(tokens=find_budget),
        model=model,
        timeout_ms=FRAME_TIMEOUT_MS,
    )
    calls, tokens = _spent(results)
    stats.update(find_frames=len(frames), frames=calls, tokens=tokens)
    for (reviewer, chunk), result in zip(frames, results):
        failure = _failure(result)
        if failure:
            failures.setdefault(failure, []).append(f"{reviewer.key} on {chunk.path}")
            continue
        source = scope.read_file(chunk.path)
        raised += normalize_findings(result, reviewer, chunk, len(source) if source else None)
    for failure, where in failures.items():
        not_checked.append(
            f"{len(where)} reviewer passes {failure}: "
            + ", ".join(where[:8])
            + (" ..." if len(where) > 8 else "")
        )
    return raised, failures


def _static_base_reader(git: Git | None, scope: Scope) -> Callable[[str], list[str] | None]:
    if git is not None and scope.base and scope.base != EMPTY_TREE and not scope.base.startswith("-"):
        return _rev_reader(git, scope.base)
    return lambda _path: None


def _is_static_duplicate(finding: dict[str, Any], all_static: list[dict[str, Any]]) -> bool:
    for item in all_static:
        if item["file"] != finding["file"]:
            continue
        if item["line"] == finding["line"]:
            return True
        if abs(item["line"] - finding["line"]) <= DEDUPE_WINDOW and similar_claims(item["claim"], finding["claim"]):
            return True
    return False


def _filter_static_findings(
    merged: list[dict[str, Any]],
    all_static: list[dict[str, Any]],
    static_checks: Any,
    static_out: dict[str, Any],
) -> list[dict[str, Any]]:
    filtered: list[dict[str, Any]] = []
    for finding in merged:
        if static_checks.contradiction(finding, static_out) is not None:
            continue
        if _is_static_duplicate(finding, all_static):
            continue
        filtered.append(finding)
    return filtered


async def _run_static_stage(
    rlm: Any,
    git: Git | None,
    scope: Scope,
    merged: list[dict[str, Any]],
    not_checked: list[str],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[str]]:
    found_static: list[dict[str, Any]] = []
    static_ran: list[str] = []
    try:
        import autoreview_static as static_checks

        read_base = _static_base_reader(git, scope)
        static_out = await static_checks.run_static(rlm, scope.files, scope.read_file, read_base)
        not_checked += static_checks.notes(static_out)
        all_static = list(static_out.get("findings") or []) + static_checks.orphaned_private_helpers(git, scope)
        found_static = [item for item in all_static if item.get("host_confirmed")]
        unconfirmed_static = [item for item in all_static if not item.get("host_confirmed")]
        static_ran = [s["name"] for s in static_out.get("stages") or [] if s.get("status") in ("clean", "findings")]
        filtered = _filter_static_findings(merged, all_static, static_checks, static_out)
        return [*filtered, *unconfirmed_static], found_static, static_ran
    except Exception as error:  # noqa: BLE001 - static stage degrades cleanly
        not_checked.append(f"Static checks did not run ({_text(f'{type(error).__name__}: {error}', 160)}).")
        return merged, found_static, static_ran


def _reviewer_context_lines(finding: dict[str, Any], key: str, mapping: dict[str, list[str]]) -> Sequence[str]:
    if key in finding["reviewers"] and mapping.get(finding["file"]):
        return mapping[finding["file"]]
    return ()


def _build_verify_item(
    git: Git | None,
    scope: Scope,
    finding: dict[str, Any],
    by_path: dict[str, FileDiff],
    reuse: dict[str, list[str]],
    contracts: dict[str, list[str]],
    conventions: dict[str, list[str]],
) -> tuple[list[str], str, int | None]:
    from review_shared import build_verify_context

    source = scope.read_file(finding["file"])
    window = source_window(source, finding["line"], line_limit=1_200)
    hunk = hunk_for(by_path.get(finding["file"]), finding["line"])
    hunk_text = "\n".join(render_hunk(hunk)) if hunk else "(no hunk)"
    related = related_code(git, scope, finding, source) if git is not None else ""
    extra_views: list[str] = []
    reuse_lines = _reviewer_context_lines(finding, REUSE_KEY, reuse)
    if reuse_lines:
        extra_views.append(
            "Existing definitions with similar names, and added lines that speak of compatibility "
            "(found by a search of the repository):\n" + "\n".join(reuse_lines)
        )
    contract_lines = _reviewer_context_lines(finding, CONTRACT_KEY, contracts)
    conv_blocks = _reviewer_context_lines(finding, CONVENTIONS_KEY, conventions)
    read = needed_code(git, scope, finding.get("read")) if git is not None else ""
    _, views, source_text = build_verify_context(
        finding,
        window=window,
        hunk_text=hunk_text,
        related=related,
        conventions_blocks=conv_blocks,
        contract_lines=contract_lines,
        read_code=read,
        extra_views=extra_views,
    )
    return views, source_text, (len(source) if source else None)


def _prepare_verify_batch(
    git: Git | None,
    scope: Scope,
    merged: list[dict[str, Any]],
    verify_budget: int,
    reuse: dict[str, list[str]],
    contracts: dict[str, list[str]],
    conventions: dict[str, list[str]],
) -> tuple[list[list[str]], list[str], list[int | None], list[dict[str, Any]], list[dict[str, Any]]]:
    by_path = {item.path: item for item in scope.files}
    items: list[list[str]] = []
    sources: list[str] = []
    counts: list[int | None] = []
    to_verify: list[dict[str, Any]] = []
    unverified: list[dict[str, Any]] = []
    estimate = 0
    for finding in merged:
        views, source_text, count = _build_verify_item(git, scope, finding, by_path, reuse, contracts, conventions)
        cost = _estimate_tokens(VERIFIER_TASK, *views, output=600)
        if estimate + cost > verify_budget:
            unverified.append(dict(finding, verification="not verified (budget)"))
            continue
        estimate += cost
        items.append(views)
        sources.append(source_text)
        counts.append(count)
        to_verify.append(finding)
    return items, sources, counts, to_verify, unverified


def _collect_followup_indices(
    git: Git | None,
    scope: Scope,
    verdicts: list[Any],
    given: dict[int, list[str]],
) -> list[int]:
    again: list[int] = []
    for index, verdict in enumerate(verdicts):
        if not isinstance(verdict, dict) or verdict.get("verdict") != "uncertain":
            continue
        code = needed_code(git, scope, verdict.get("needs")) if git is not None else ""
        if not code or code in given.get(index, []):
            continue
        given.setdefault(index, []).append(code)
        again.append(index)
    return again


async def _run_followup_rounds(
    rlm: Any,
    git: Git | None,
    scope: Scope,
    items: list[list[str]],
    verdicts: list[Any],
    sources: list[str],
    *,
    left: int,
    model: str | None,
    stats: dict[str, Any],
) -> None:
    given: dict[int, list[str]] = {}
    for _ in range(NEED_ROUNDS):
        again = _collect_followup_indices(git, scope, verdicts, given)
        if not again or left <= 0:
            break
        answers = await rlm.map(
            VERIFIER_TASK,
            [[*items[index], "Code you asked for:\n" + "\n\n".join(given[index])] for index in again],
            contract=NEEDS_CONTRACT,
            budget=Budget(tokens=left),
            model=model,
            timeout_ms=FRAME_TIMEOUT_MS,
        )
        calls, tokens = _spent(answers)
        left -= tokens
        stats["verify_frames"] += len(again)
        stats["frames"] += calls
        stats["tokens"] += tokens
        for index, answer in zip(again, answers):
            if isinstance(answer, dict):
                verdicts[index] = answer
                sources[index] += "\n" + given[index][-1]


async def _run_verify_stage(
    rlm: Any,
    git: Git | None,
    scope: Scope,
    merged: list[dict[str, Any]],
    *,
    verify_budget: int,
    model: str | None,
    reuse: dict[str, list[str]],
    contracts: dict[str, list[str]],
    conventions: dict[str, list[str]],
    stats: dict[str, Any],
    not_checked: list[str],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    items, sources, counts, to_verify, unverified = _prepare_verify_batch(
        git, scope, merged, verify_budget, reuse, contracts, conventions
    )
    confirmed: list[dict[str, Any]] = []
    uncertain: list[dict[str, Any]] = []
    rejected: list[dict[str, Any]] = []
    if to_verify:
        verdicts = list(
            await rlm.map(
                VERIFIER_TASK,
                items,
                contract=NEEDS_CONTRACT,
                budget=Budget(tokens=verify_budget),
                model=model,
                timeout_ms=FRAME_TIMEOUT_MS,
            )
        )
        calls, tokens = _spent(verdicts)
        stats["verify_frames"] = len(to_verify)
        stats["frames"] += calls
        stats["tokens"] += tokens
        await _run_followup_rounds(
            rlm,
            git,
            scope,
            items,
            verdicts,
            sources,
            left=verify_budget - tokens,
            model=model,
            stats=stats,
        )
        confirmed, uncertain, rejected = apply_verdicts(to_verify, list(verdicts), sources, counts)
    uncertain += unverified
    if unverified:
        not_checked.append(f"{len(unverified)} findings were not verified within the budget (listed as uncertain).")
    return confirmed, uncertain, rejected, unverified


async def _children(rlm: Any, handles: list[Any], timeout_ms: int) -> dict[Any, dict[str, Any]]:
    """The results of the sub-agents that ended within `timeout_ms`, by id. `rlm.collect` is not used: in a root
    cell it does not wait (the host waits between turns), and a review is one cell.
    ponytail: asks the host every two seconds; a host-side wait usable from a root cell would replace the loop."""
    deadline = time.monotonic() + timeout_ms / 1000
    while True:
        state = await rlm.results(handles)
        if not state.get("running") or time.monotonic() >= deadline:
            return {
                entry.get("id"): entry.get("result") or {}
                for entry in state.get("results", [])
                if isinstance(entry, dict)
            }
        await asyncio.sleep(CHILD_POLL_S)


async def _explore_start(
    rlm: Any,
    scope: Scope,
    chunks: list[Chunk],
    contracts: dict[str, list[str]],
) -> list[tuple[Any, Chunk]]:
    """Start the sub-agents that read past the diff. A frame judges the lines it is given; these follow a changed
    file into what it calls and what it replaces, which no search by name finds. They take the host's sub-agent
    model (`/settings -> Models`), which is where a cheap one is set."""
    first: dict[str, Chunk] = {}
    for chunk in chunks:
        if chunk.kind != "doc":
            first.setdefault(chunk.path, chunk)
    files = sorted((item for item in scope.files if item.path in first), key=lambda item: -item.added)[:EXPLORERS]
    started = []
    for number, item in enumerate(files, 1):
        hunks = "\n".join(line for hunk in item.hunks for line in render_hunk(hunk))[:EXPLORE_HUNK_CHARS]
        known = contracts.get(item.path)
        found = ("\nA search found this about the endpoints it talks to:\n" + "\n".join(known) + "\n") if known else ""
        brief = EXPLORE_BRIEF.format(cwd=scope.root, path=item.path, hunks=hunks, contracts=found)
        handle = await rlm.spawn(brief, name=f"review-explore-{number}", timeout_ms=EXPLORE_TIMEOUT_MS)
        started.append((handle, first[item.path]))
    return started


async def _explore_findings(
    rlm: Any,
    scope: Scope,
    started: list[tuple[Any, Chunk]],
) -> tuple[list[dict[str, Any]], list[str]]:
    """What the sub-agents found, as findings for the same dedupe and verifier as a frame's (each with the code its
    sub-agent read, under "read"), and the files whose sub-agent gave no answer."""
    by_id = await _children(rlm, [handle for handle, _ in started], EXPLORE_TIMEOUT_MS + 30_000)
    reviewer = Reviewer(EXPLORE_KEY, "Past the diff", "", ())
    found: list[dict[str, Any]] = []
    silent: list[str] = []
    for handle, chunk in started:
        result = by_id.get(getattr(handle, "rlm_child_id", handle), {})
        value = parse_json_object(result.get("value"), "findings") if result.get("status") == "succeeded" else None
        raw = value.get("findings") if value else None
        if not isinstance(raw, list):
            silent.append(chunk.path)
            continue
        source = scope.read_file(chunk.path)
        for item in raw[:MAX_FINDINGS_PER_FRAME]:
            for finding in normalize_findings([item], reviewer, chunk, len(source) if source else None):
                found.append(dict(finding, read=item.get("read")))
    return found, silent


def _select_deep_candidates(
    options: Options,
    uncertain: list[dict[str, Any]],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    ranked = sorted(uncertain, key=_rank)
    # By default every blocker and major: a serious finding is not left undecided for want of a look.
    if options.deep is None:
        chosen = [finding for finding in ranked if finding["severity"] in ("blocker", "major")][:DEEP_MAX]
    else:
        chosen = ranked[: options.deep]
    rest = [finding for finding in uncertain if finding not in chosen]
    return chosen, rest


async def _spawn_deep_verifiers(rlm: Any, cwd: str, chosen: list[dict[str, Any]]) -> list[Any]:
    handles = []
    for index, finding in enumerate(chosen, 1):
        public = {key: finding[key] for key in ("file", "line", "severity", "category", "claim", "why")}
        brief = DEEP_VERIFY_BRIEF.format(
            cwd=cwd,
            finding=json.dumps(public, indent=1),
            note=finding.get("verification") or "no verdict",
        )
        handles.append(await rlm.spawn(brief, name=f"review-check-{index}", timeout_ms=DEEP_TIMEOUT_MS))
    return handles


def _collect_deep_verdicts(
    scope: Scope,
    chosen: list[dict[str, Any]],
    handles: list[Any],
    by_id: dict[Any, dict[str, Any]],
) -> tuple[list[Any], list[str], list[int | None]]:
    verdicts = []
    for handle in handles:
        result = by_id.get(getattr(handle, "rlm_child_id", handle), {})
        value = parse_json_object(result.get("value")) if result.get("status") == "succeeded" else None
        verdicts.append(value)
    sources = []
    counts = []
    for finding in chosen:
        source = scope.read_file(finding["file"])
        sources.append("\n".join(source) if source else "")
        counts.append(len(source) if source else None)
    return verdicts, sources, counts


async def _deep_verify(
    rlm: Any,
    options: Options,
    cwd: str,
    scope: Scope,
    confirmed: list[dict[str, Any]],
    uncertain: list[dict[str, Any]],
    rejected: list[dict[str, Any]],
) -> tuple[Any, Any, Any, int]:
    """Re-check the most severe uncertain findings with sub-agents that can read the whole repository."""
    chosen, rest = _select_deep_candidates(options, uncertain)
    handles = await _spawn_deep_verifiers(rlm, cwd, chosen)
    if not handles:
        return confirmed, uncertain, rejected, 0
    by_id = await _children(rlm, handles, DEEP_TIMEOUT_MS + 30_000)
    verdicts, sources, counts = _collect_deep_verdicts(scope, chosen, handles, by_id)
    more_confirmed, still_uncertain, more_rejected = apply_verdicts(chosen, verdicts, sources, counts)
    return confirmed + more_confirmed, rest + still_uncertain, rejected + more_rejected, len(handles)


# Reports posted from this kernel, as (resolved path, PR number).
_POSTED: set[tuple[str, int]] = set()


def _validate_post_request(
    review: Review | str,
    confirm: bool,
    pr: int | None,
    which: Callable[[str], str | None],
) -> tuple[str, int, tuple[str, int]]:
    if confirm is not True:
        raise ReviewError("posting needs confirm=True, and only after the user explicitly agreed to post")
    path = review if isinstance(review, str) else review.path
    number = pr if pr is not None else (None if isinstance(review, str) else review.pr)
    if number is None:
        raise ReviewError("this review has no pull request to post to")
    if not path or not Path(path).is_file():
        raise ReviewError("the review has no saved report to post")
    posted = (str(Path(path).resolve()), number)
    if posted in _POSTED:
        raise ReviewError(f"this report was already posted to PR #{number}")
    if which("gh") is None:
        raise ReviewError("posting needs the GitHub CLI (gh)")
    return path, number, posted


async def post(
    review: Review | str,
    *,
    confirm: bool = False,
    pr: int | None = None,
    runner: Runner | None = None,
    which: Callable[[str], str | None] = shutil.which,
    cwd: str | None = None,
) -> str:
    """Post a review's saved report as a PR comment with `gh`. Call only after the user explicitly said yes.
    `review` is a Review, or the saved report's path together with `pr=`. A report is posted once: a second call
    (a retry after it went through) is refused."""
    path, number, posted = _validate_post_request(review, confirm, pr, which)
    code, stdout, stderr = (runner or _run_process)(
        ["gh", "pr", "comment", str(number), "--body-file", path],
        cwd or os.getcwd(),
        GH_TIMEOUT_S,
    )
    if code != 0:
        raise ReviewError(f"gh pr comment failed: {(stderr or stdout).strip()[:300]}")
    _POSTED.add(posted)
    if isinstance(review, Review):
        review.post_pending = False
    return stdout.strip() or f"posted to PR #{number}"


async def _init_review_run(
    rlm: Any,
    args: str | list[str] | None,
    cwd: str,
    runner: Runner | None,
    which: Callable[[str], str | None],
) -> tuple[Options, Scope] | Review:
    try:
        options = parse_args(args, dict(os.environ))
    except ReviewError as error:
        return Review(f"/review: {error}\n")
    if options.help:
        return Review(USAGE + "\n")
    if options.model is None:
        options.frame_model = await _saved_review_model(rlm)
    try:
        scope = resolve_scope(options, cwd, runner=runner, which=which)
    except ReviewError as error:
        return Review(f"/review: {error}\n")
    return options, scope


async def _build_reviewers_list(rlm: Any, git: Git, scope: Scope, options: Options) -> list[Reviewer]:
    reviewers = options.reviewers
    full_run = options.only is None and not options.plan
    if full_run and scope.files:
        lessons = await _lessons(rlm)
        mem_items = await recall_memorantado_items(rlm, scope.files, label=scope.label)
        lessons = [*lessons, *_knowledge_rules(git, scope.files), *mem_items]
        if lessons:
            reviewers = [*reviewers, lessons_reviewer(lessons)]
    if scope.whole and full_run:
        accepted = _acceptance_items(git)
        if accepted:
            reviewers = [*reviewers, acceptance_reviewer(accepted)]
    return reviewers


def _attach_contract_inventory(
    git: Git,
    scope: Scope,
    reviewers: list[Reviewer],
    not_checked: list[str],
) -> tuple[list[Reviewer], dict[str, list[str]]]:
    contracts, contract_notes = contract_inventory(git, scope)
    not_checked += contract_notes
    if not contracts:
        return reviewers, contracts
    reviewers = [*reviewers, contract_reviewer(contracts)]
    if scope.whole and scope.pr is None and (ids := _run_items(git, contracts)):
        not_checked.append(
            f"Added to the spec's acceptance list ({', '.join(ids)}): each endpoint this "
            "change calls, to be shown with a run against its real handler, which no reading "
            "of code shows. Mark each with spec_api.mark and what the run showed."
        )
    return reviewers, contracts


def _attach_unselected_and_ci_notes(
    scope: Scope,
    reviewers: list[Reviewer],
    not_checked: list[str],
    *,
    include_ci: bool,
) -> None:
    if include_ci and scope.pr is None and (commands := ci_commands(scope.root)):
        not_checked.append(
            "CI runs these, and this review ran none of them; run each before pushing: "
            + ", ".join(f"`{_clip(command, 120)}`" for command in commands)
            + "."
        )
    left_out = [reviewer.title for reviewer in REVIEWERS.values() if reviewer not in reviewers]
    if left_out:
        not_checked.append(f"Reviewers not selected: {', '.join(left_out)}.")


def _collect_inventories(
    git: Git,
    scope: Scope,
    options: Options,
    chunks: list[Chunk],
    reviewers: list[Reviewer],
    not_checked: list[str],
) -> tuple[list[Reviewer], dict[str, list[str]], dict[str, list[str]], dict[str, list[str]]]:
    reuse: dict[str, list[str]] = {}
    contracts: dict[str, list[str]] = {}
    conventions: dict[str, list[str]] = {}
    include_inventories = bool(chunks and options.only is None and not options.plan)
    if include_inventories:
        reuse, reuse_notes = reuse_inventory(git, scope)
        not_checked += reuse_notes
        if reuse:
            reviewers = [*reviewers, reuse_reviewer(reuse)]
        reviewers, contracts = _attach_contract_inventory(git, scope, reviewers, not_checked)
        conventions, conv_notes = conventions_inventory(git, scope, base=scope.base)
        not_checked += conv_notes
        if conventions:
            reviewers = [*reviewers, conventions_reviewer(conventions)]
    _attach_unselected_and_ci_notes(scope, reviewers, not_checked, include_ci=include_inventories)
    return reviewers, reuse, contracts, conventions


def _empty_scope_review(git: Git, scope: Scope, options: Options, skipped: list[tuple[str, str]]) -> Review:
    reason = "No changes to review" if not scope.files else "No reviewable changes"
    report = f"# Code review: {scope.label}\n\n{reason}.\n"
    if skipped:
        report += "\n" + "\n".join(f"- skipped {path}: {why}" for path, why in skipped) + "\n"
    receipt_line = _write_receipt(git, scope, options, 0, False)
    if receipt_line:
        report += f"\n{receipt_line}\n"
    return Review(report, scope=scope.label, pr=scope.pr)


def _record_plan_notes(reviewers: list[Reviewer], plan: dict[str, Any], not_checked: list[str]) -> None:
    for reviewer_key, count in plan["not_applicable"].items():
        reviewer = next(item for item in reviewers if item.key == reviewer_key)
        why = "no LLM-related code" if reviewer.llm_only else "documentation only"
        not_checked.append(f"{reviewer.title} reviewer skipped {count} chunk(s) with {why}.")
    if plan["dropped"]:
        paths = sorted({chunk.path for _, chunk in plan["dropped"]})
        not_checked.append(
            f"{len(plan['dropped'])} reviewer passes did not fit the budget (raise --budget): "
            + ", ".join(paths[:10])
            + (" ..." if len(paths) > 10 else "")
        )


def _plan_review_budget(
    scope: Scope,
    options: Options,
    chunks: list[Chunk],
    reviewers: list[Reviewer],
) -> tuple[int, dict[str, Any]]:
    if options.budget_tokens is None:
        options.budget_tokens = DEFAULT_BUDGET_TOKENS
        if scope.whole and scope.pr is None and options.only is None:
            # This is the review a pull request waits on: a pass dropped for budget is a file nobody read.
            need = plan_find(chunks, reviewers, AUTO_BUDGET_MAX)["estimate"] * AUTO_BUDGET_HEADROOM / FIND_SHARE
            options.budget_tokens = min(AUTO_BUDGET_MAX, max(DEFAULT_BUDGET_TOKENS, int(need)))
    find_budget = int(options.budget_tokens * FIND_SHARE)
    return find_budget, plan_find(chunks, reviewers, find_budget)


async def _start_explorers_safe(
    rlm: Any,
    scope: Scope,
    chunks: list[Chunk],
    contracts: dict[str, list[str]],
    not_checked: list[str],
) -> list[tuple[Any, Chunk]]:
    try:
        return await _explore_start(rlm, scope, chunks, contracts)
    except Exception as error:
        not_checked.append(f"No sub-agent read past the diff ({_text(str(error) or type(error).__name__, 160)}).")
        return []


async def _collect_explorers_safe(
    rlm: Any,
    scope: Scope,
    exploring: list[tuple[Any, Chunk]],
    reviewers: list[Reviewer],
    stats: dict[str, Any],
    not_checked: list[str],
) -> tuple[list[dict[str, Any]], list[Reviewer]]:
    if not exploring:
        return [], reviewers
    stats["subagents"] = len(exploring)
    reviewers = [*reviewers, Reviewer(EXPLORE_KEY, f"Past the diff ({len(exploring)} sub-agents)", "", ())]
    try:
        explored, silent = await _explore_findings(rlm, scope, exploring)
    except Exception as error:
        not_checked.append(
            f"The sub-agents reading past the diff were not heard ({_text(str(error) or type(error).__name__, 160)})."
        )
        return [], reviewers
    if silent:
        not_checked.append(
            f"{len(silent)} sub-agents reading past the diff gave no answer: "
            + ", ".join(silent[:8])
            + (" ..." if len(silent) > 8 else "")
        )
    return explored, reviewers


async def _run_sweep_if_whole(
    rlm: Any,
    git: Git,
    scope: Scope,
    options: Options,
    chunks: list[Chunk],
    raised: list[dict[str, Any]],
    merged: list[dict[str, Any]],
    find_budget: int,
    stats: dict[str, Any],
) -> list[dict[str, Any]]:
    if not (scope.whole and scope.pr is None and options.only is None and chunks):
        return merged
    sweep_budget = max(0, find_budget - stats["tokens"])
    swept, sweep_frames, sweep_tokens = await _gap_sweep(
        rlm,
        scope,
        chunks,
        merged,
        options.model or options.frame_model,
        sweep_budget,
        git=git,
    )
    if sweep_frames:
        stats["find_frames"] += sweep_frames
        stats["frames"] += sweep_frames
        stats["tokens"] += sweep_tokens
    if swept:
        raised += swept
        stats["raised"] = len(raised)
        return dedupe(raised)
    return merged


async def _run_deep_verify_safe(
    rlm: Any,
    options: Options,
    scope: Scope,
    confirmed: list[dict[str, Any]],
    uncertain: list[dict[str, Any]],
    rejected: list[dict[str, Any]],
    stats: dict[str, Any],
    not_checked: list[str],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    try:
        confirmed, uncertain, rejected, spawned = await _deep_verify(
            rlm, options, scope.root, scope, confirmed, uncertain, rejected
        )
        stats["subagents"] += spawned
    except Exception as error:
        not_checked.append(
            f"No sub-agent re-checked the undecided findings ({_text(str(error) or type(error).__name__, 160)})."
        )
    return confirmed, uncertain, rejected


def _split_and_sort_confirmed(
    confirmed: list[dict[str, Any]],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    preexisting = [finding for finding in confirmed if finding.get("introduced") == "before"]
    current = [finding for finding in confirmed if finding.get("introduced") != "before"]
    current.sort(key=_rank)
    preexisting.sort(key=_rank)
    return current, preexisting


def _count_changed_lines(scope: Scope) -> int:
    if not (scope.whole and scope.pr is None):
        return 0
    # Generated files, lockfiles, binaries and deletions are not read, so they are not counted either.
    return sum(item.added + item.removed for item in scope.files if skip_reason(item) is None)


async def _finalize_review_run(
    rlm: Any,
    git: Git,
    scope: Scope,
    options: Options,
    reviewers: list[Reviewer],
    chunks: list[Chunk],
    confirmed: list[dict[str, Any]],
    uncertain: list[dict[str, Any]],
    rejected: list[dict[str, Any]],
    stats: dict[str, Any],
    not_checked: list[str],
    static_ran: list[str],
    incomplete: bool,
) -> Review:
    confirmed, preexisting = _split_and_sort_confirmed(confirmed)
    purposes = await _purposes(rlm, scope.files, chunks, options.model or options.frame_model) if scope.whole else []
    holding = sum(finding["severity"] in ("blocker", "major") for finding in confirmed)
    lines = _count_changed_lines(scope)
    receipt_line = _write_receipt(git, scope, options, holding, incomplete, purposes, len(confirmed) - holding, lines)
    if receipt_line:
        not_checked.append(receipt_line)
    report_args = dict(
        label=scope.label,
        files=scope.files,
        reviewers=reviewers,
        confirmed=confirmed,
        uncertain=uncertain,
        rejected=rejected,
        stats=stats,
        not_checked=not_checked,
        read_file=scope.read_file,
        purposes=purposes,
        lines=lines,
        preexisting=preexisting,
        static_ran=static_ran,
    )
    return _finish(report_args, git, scope.pr, options.post)


async def _run_find_and_sweep(
    rlm: Any,
    git: Git,
    scope: Scope,
    options: Options,
    reviewers: list[Reviewer],
    chunks: list[Chunk],
    plan: dict[str, Any],
    contracts: dict[str, list[str]],
    find_budget: int,
    children: bool,
    stats: dict[str, Any],
    not_checked: list[str],
) -> tuple[list[dict[str, Any]], list[Reviewer], dict[str, list[str]]]:
    exploring = await _start_explorers_safe(rlm, scope, chunks, contracts, not_checked) if children else []
    model = options.model or options.frame_model
    raised, failures = await _run_find_stage(
        rlm, scope, plan["frames"], find_budget=find_budget, model=model, stats=stats, not_checked=not_checked
    )
    explored, reviewers = await _collect_explorers_safe(rlm, scope, exploring, reviewers, stats, not_checked)
    raised += explored
    stats["raised"] = len(raised)
    merged = await _run_sweep_if_whole(rlm, git, scope, options, chunks, raised, dedupe(raised), find_budget, stats)
    stats["merged"] = len(merged)
    return merged, reviewers, failures


async def _run_post_verify_stages(
    rlm: Any,
    git: Git,
    scope: Scope,
    options: Options,
    chunks: list[Chunk],
    confirmed: list[dict[str, Any]],
    uncertain: list[dict[str, Any]],
    rejected: list[dict[str, Any]],
    found_static: list[dict[str, Any]],
    children: bool,
    stats: dict[str, Any],
    not_checked: list[str],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    if children and uncertain:
        confirmed, uncertain, rejected = await _run_deep_verify_safe(
            rlm, options, scope, confirmed, uncertain, rejected, stats, not_checked
        )
    if found_static:
        confirmed = [*found_static, *confirmed]
    if scope.whole and scope.pr is None and options.only is None and chunks:
        test_findings = _run_sandboxed_tests(git, scope, not_checked)
        if test_findings:
            confirmed = [*test_findings, *confirmed]
    return confirmed, uncertain, rejected


async def _execute_review_plan(
    rlm: Any,
    git: Git,
    scope: Scope,
    options: Options,
    reviewers: list[Reviewer],
    chunks: list[Chunk],
    plan: dict[str, Any],
    reuse: dict[str, list[str]],
    contracts: dict[str, list[str]],
    conventions: dict[str, list[str]],
    find_budget: int,
    not_checked: list[str],
) -> Review:
    _record_plan_notes(reviewers, plan, not_checked)
    stats: dict[str, Any] = {
        "budget": options.budget_tokens,
        "frames": 0,
        "tokens": 0,
        "find_frames": 0,
        "verify_frames": 0,
        "subagents": 0,
    }
    # Sub-agents: on for a review of the whole local change, where the repository is there to read; --deep=0 is off.
    local = scope.pr is None and options.only is None
    children = local and (scope.whole if options.deep is None else options.deep > 0)
    merged, reviewers, failures = await _run_find_and_sweep(
        rlm, git, scope, options, reviewers, chunks, plan, contracts, find_budget, children, stats, not_checked
    )
    found_static: list[dict[str, Any]] = []
    static_ran: list[str] = []
    if chunks and scope.pr is None:
        merged, found_static, static_ran = await _run_static_stage(rlm, git, scope, merged, not_checked)
    confirmed, uncertain, rejected, unverified = await _run_verify_stage(
        rlm,
        git,
        scope,
        merged,
        verify_budget=max(0, (options.budget_tokens or 0) - stats["tokens"]),
        model=options.model or options.frame_model,
        reuse=reuse,
        contracts=contracts,
        conventions=conventions,
        stats=stats,
        not_checked=not_checked,
    )
    confirmed, uncertain, rejected = await _run_post_verify_stages(
        rlm, git, scope, options, chunks, confirmed, uncertain, rejected, found_static, children, stats, not_checked
    )
    # A review that could not finish is not a pass: passes cut by the budget or failed, findings left unverified.
    incomplete = bool(plan["dropped"] or unverified or failures)
    return await _finalize_review_run(
        rlm, git, scope, options, reviewers, chunks, confirmed, uncertain, rejected, stats,
        not_checked, static_ran, incomplete,
    )


__all__ = [name for name in globals() if not name.startswith("__")]
