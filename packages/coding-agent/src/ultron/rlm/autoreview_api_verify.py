"""Verification candidate filtering, view construction, job batching, and verifier execution."""
from __future__ import annotations

import json
from typing import Any

import autoreview_compiled as compiled
import autoreview_deep as deep
import autoreview_static as static_checks
from autoreview_api_guides import raised_by_others
from autoreview_api_recheck import RECHECK_LINE_CHARS
from autoreview_api_rules import (
    AUTOREVIEW_VERDICT_BATCH_CONTRACT,
    AUTOREVIEW_VERDICT_CONTRACT,
    check_view,
    duplicate_record,
)
from infer_api import FrameError, Incomplete
from review_api import (
    CONVENTIONS_KEY,
    FileDiff,
    Git,
    Scope,
    hunk_for,
    needed_code,
    related_code,
    render_hunk,
    similar_claims,
    source_window,
)
from review_prompts import AUTOREVIEW_VERIFIER_BATCH_TASK, AUTOREVIEW_VERIFIER_TASK

EARLIER_WINDOW = 5
DEFAULT_VERIFY_BATCH = 4
MAX_VERIFY_BATCH = 8
VERIFY_SOLO_CHARS = 6_000


def _find_static_twin(
    finding: dict[str, Any], found_static: list[dict[str, Any]]
) -> dict[str, Any] | None:
    for item in found_static:
        if item["file"] != finding["file"]:
            continue
        if item["line"] == finding["line"]:
            return item
        close_line = abs(item["line"] - finding["line"]) <= EARLIER_WINDOW
        if close_line and similar_claims(item["claim"], finding["claim"]):
            return item
    return None


def _find_earlier_twin(
    finding: dict[str, Any], open_earlier: list[dict[str, Any]]
) -> dict[str, Any] | None:
    for item in open_earlier:
        if item["file"] != finding["file"]:
            continue
        if abs(item["line"] - finding["line"]) <= EARLIER_WINDOW and similar_claims(
            item["claim"], finding["claim"]
        ):
            return item
    return None


def _check_static_filter(
    finding: dict[str, Any],
    static_ids: set[int],
    found_static: list[dict[str, Any]],
    static_out: dict[str, Any] | None,
    contradicted: list[dict[str, Any]],
    duplicate_records: list[dict[str, Any]],
) -> tuple[bool, int]:
    if id(finding) in static_ids or finding.get("host_confirmed"):
        return False, 0

    why = static_checks.contradiction(finding, static_out)
    if why is not None:
        contradicted.append(
            {
                "file": finding["file"],
                "line": finding["line"],
                "claim": finding["claim"],
                "source": finding.get("source") or "fast",
                "by": why,
            }
        )
        return True, 0

    twin = _find_static_twin(finding, found_static)
    if twin is not None:
        duplicate_records.append(duplicate_record(finding, twin, "static"))
        return True, 1
    return False, 0


def _classify_hybrid_verification(
    finding: dict[str, Any], hybrid_confirmed: list[dict[str, Any]]
) -> tuple[bool, int, int]:
    state = finding["verification"]["state"]
    if state == "confirmed":
        hybrid_confirmed.append(finding)
        return True, 0, 0
    if state in ("refuted", "dropped"):
        return True, 1, 0
    finding["check"] = finding.pop("verification")
    return False, 0, 1


def _also_raised_entry(finding: dict[str, Any], logins: list[str]) -> dict[str, Any]:
    return {
        "file": finding["file"],
        "line": finding["line"],
        "severity": finding["level"],
        "claim": finding["claim"],
        "by": logins,
    }


def _filter_verify_candidates(
    merged: list[dict[str, Any]],
    found_static: list[dict[str, Any]],
    static_out: dict[str, Any] | None,
    others: list[dict[str, Any]],
    open_earlier: list[dict[str, Any]],
    hybrid: bool,
    duplicate_records: list[dict[str, Any]],
) -> tuple[
    list[dict[str, Any]],
    list[dict[str, Any]],
    list[dict[str, Any]],
    list[dict[str, Any]],
    int,
    int,
    int,
]:
    for number, finding in enumerate(found_static, len(merged) + 1):
        finding["id"] = number

    static_ids = {id(item) for item in found_static}
    also_raised: list[dict[str, Any]] = []
    candidates: list[dict[str, Any]] = []
    hybrid_confirmed: list[dict[str, Any]] = []
    contradicted: list[dict[str, Any]] = []
    duplicates = 0
    hybrid_refuted = 0
    hybrid_undecided = 0

    for finding in merged + found_static:
        dropped_static, static_dupes = _check_static_filter(
            finding, static_ids, found_static, static_out, contradicted, duplicate_records
        )
        duplicates += static_dupes
        if dropped_static:
            continue

        logins = raised_by_others(finding, others)
        if logins:
            also_raised.append(_also_raised_entry(finding, logins))
            continue

        earlier_twin = _find_earlier_twin(finding, open_earlier)
        if earlier_twin is not None:
            duplicates += 1
            annotated = dict(earlier_twin, source="earlier review", category=finding["category"])
            duplicate_records.append(duplicate_record(finding, annotated, "earlier"))
            continue

        if hybrid and isinstance(finding.get("verification"), dict):
            handled, refuted_delta, undecided_delta = _classify_hybrid_verification(
                finding, hybrid_confirmed
            )
            hybrid_refuted += refuted_delta
            hybrid_undecided += undecided_delta
            if handled:
                continue

        candidates.append(finding)

    return (
        candidates,
        also_raised,
        contradicted,
        hybrid_confirmed,
        duplicates,
        hybrid_refuted,
        hybrid_undecided,
    )


def _sum_batch_field(verify_batches: list[dict[str, Any]], key: str) -> int:
    return sum(int(batch.get(key, 0)) for batch in verify_batches)


def _sum_batch_stat(verify_batches: list[dict[str, Any]], key: str) -> int:
    return sum(int(batch["stats"][key]) for batch in verify_batches)


def _sum_batch_nested_stat(
    verify_batches: list[dict[str, Any]], group: str, keys: tuple[str, ...]
) -> dict[str, int]:
    return {
        key: sum(int(batch["stats"][group][key]) for batch in verify_batches)
        for key in keys
    }


def _collect_batch_shapes(verify_batches: list[dict[str, Any]]) -> dict[str, int]:
    shapes: dict[str, int] = {}
    for batch in verify_batches:
        for shape, count in batch["shapes"].items():
            shapes[shape] = shapes.get(shape, 0) + count
    return shapes


def _build_hybrid_program_stats(
    verify_batches: list[dict[str, Any]], prepared: dict[str, Any] | None
) -> dict[str, Any]:
    retrieval = (
        {key: prepared["retrieval"].get(key) for key in ("items", "chars", "ms", "reference")}
        if prepared
        else None
    )
    return {
        "planned": _sum_batch_stat(verify_batches, "planned"),
        "expanded": 0,
        "executed": _sum_batch_stat(verify_batches, "executed"),
        "failed": _sum_batch_stat(verify_batches, "failed"),
        "skipped": _sum_batch_stat(verify_batches, "skipped"),
        "asks": _sum_batch_stat(verify_batches, "asks"),
        "autoAsks": _sum_batch_stat(verify_batches, "autoAsks"),
        "tests": _sum_batch_stat(verify_batches, "tests"),
        "checks": _sum_batch_nested_stat(
            verify_batches, "checks", ("held", "failed", "unknown", "contradicted")
        ),
        "findings": _sum_batch_nested_stat(
            verify_batches, "findings", ("deterministic", "asked", "resolved", "dropped", "refuted")
        ),
        "truncated": [],
        "limits": {"planned": compiled.MAX_PROGRAM_STEPS, "expanded": compiled.MAX_STEPS},
        "planner": {
            "ms": sum(batch["planner"]["ms"] for batch in verify_batches),
            "tokens": sum(batch["planner"]["tokens"] for batch in verify_batches),
            "repairs": 0,
            "status": ", ".join(batch["planner"]["status"] for batch in verify_batches) or "skipped",
            "style": "hybrid",
        },
        "summary": "",
        "retrieval": retrieval,
    }


def _plural_suffix(count: int) -> str:
    return "" if count == 1 else "s"


def _hybrid_assurance_line(
    totals: dict[str, int], program_stats: dict[str, Any], hybrid_undecided: int
) -> str:
    checks_held = program_stats["checks"]["held"]
    tests_count = program_stats["tests"]
    asks_count = program_stats["asks"]
    undecided_note = (
        f" (the verifier frame judged {hybrid_undecided})" if hybrid_undecided else ""
    )
    return (
        f"Discovery raised {totals['candidates']} candidate finding{_plural_suffix(totals['candidates'])}; "
        f"the host checked {totals['checked']} with {tests_count} test run{_plural_suffix(tests_count)} and "
        f"{asks_count} small-model question{_plural_suffix(asks_count)}: "
        f"{totals['confirmed']} confirmed, {totals['refuted']} refuted, {totals['unknown']} undecided"
        f"{undecided_note}; {checks_held} check{_plural_suffix(checks_held)} held."
    )


def _append_hybrid_not_checked(
    verify_batches: list[dict[str, Any]],
    skipped_items: list[Any],
    not_checked: list[str],
) -> None:
    for batch in verify_batches:
        if batch["planner"].get("status") == "failed":
            error_text = batch["planner"].get("error", "")
            not_checked.append(
                f"The candidate planner failed for the {batch['batch']} batch ({error_text}); "
                "the template checks ran alone."
            )
    for item in skipped_items[:4]:
        not_checked.append(f"A candidate could not be checked: {item}.")


def _summarize_hybrid_verification(
    verify_batches: list[dict[str, Any]],
    hybrid_undecided: int,
    prepared: dict[str, Any] | None,
    assurance: list[str],
    not_checked: list[str],
) -> tuple[dict[str, Any], list[dict[str, Any]], dict[str, Any], list[str]]:
    totals = {
        key: _sum_batch_field(verify_batches, key)
        for key in ("candidates", "checked", "confirmed", "refuted", "unknown", "dropped", "capped")
    }
    skipped_items = [item for batch in verify_batches for item in batch["skipped"]]
    verification = {
        **totals,
        "toVerifier": hybrid_undecided,
        "shapes": _collect_batch_shapes(verify_batches),
        "planner": [batch["planner"] for batch in verify_batches],
        "batches": [
            {"batch": batch["batch"], "candidates": batch["candidates"], "ms": batch["ms"]}
            for batch in verify_batches
        ],
        "skipped": skipped_items,
    }
    program_records = [record for batch in verify_batches for record in batch["records"]]
    program_stats = _build_hybrid_program_stats(verify_batches, prepared)
    updated_assurance = [*assurance, _hybrid_assurance_line(totals, program_stats, hybrid_undecided)]
    _append_hybrid_not_checked(verify_batches, skipped_items, not_checked)
    return verification, program_records, program_stats, updated_assurance


def _closest_test_view(finding: dict[str, Any], scope: Scope) -> tuple[str, str]:
    unpinned = finding.get("unpinned") if isinstance(finding.get("unpinned"), dict) else {}
    closest = unpinned.get("closest_test")
    test_source = scope.read_file(closest["path"]) if closest else None
    if not test_source:
        return (
            "The finding names no existing test near it (or the file it names does not exist).",
            "",
        )

    test_line = min(closest["line"], len(test_source))
    test_window = source_window(test_source, test_line, 25, line_limit=RECHECK_LINE_CHARS)
    test_header = (
        f"The existing test nearest to it ({closest['path']}, around line {closest['line']}); "
        "check whether it, or a test beside it, already catches the named change:"
    )
    return f"{test_header}\n{test_window}", test_window


def _collect_extra_verify_views(
    finding: dict[str, Any], cited: str, scope: Scope
) -> tuple[list[str], str]:
    extra_views: list[str] = []
    if finding.get("test_evidence"):
        extra_views.append(
            "A test execution the investigator cites, as the host ran it in a sandbox:\n"
            + finding["test_evidence"]
        )
    if finding.get("ask_evidence"):
        extra_views.append(
            "What a small model answered when the review program asked it (untrusted; the quote was "
            f"checked to be in the material it saw):\n{finding['ask_evidence']}"
        )
    if isinstance(finding.get("check"), dict):
        extra_views.append(check_view(finding["check"]))
    if cited:
        extra_views.append(
            "Evidence the investigator cites, as the host reads it at the reviewed commit (the quoted "
            f"lines were checked to be there):\n{cited}"
        )

    test_window = ""
    if finding["category"] == "tests":
        test_view, test_window = _closest_test_view(finding, scope)
        extra_views.append(test_view)

    return extra_views, test_window


def _verify_conventions_blocks(
    finding: dict[str, Any], mode: str, conventions: dict[str, list[str]]
) -> Any:
    raw_reviewers = finding.get("reviewers")
    reviewers = raw_reviewers if isinstance(raw_reviewers, list) else ()
    if CONVENTIONS_KEY not in reviewers and mode != "minimal":
        return ()
    return conventions.get(finding["file"], ())


def _attach_public_rule_fields(
    finding: dict[str, Any], public: dict[str, Any], views: list[str]
) -> None:
    updated = False
    if finding["category"] == "tests":
        raw_unpinned = finding.get("unpinned")
        unpinned = raw_unpinned if isinstance(raw_unpinned, dict) else {}
        public["unpinned"] = {key: unpinned.get(key) for key in ("behaviour", "change")}
        updated = True
    if finding.get("consequence"):
        public["consequence"] = finding["consequence"]
        updated = True
    if updated:
        views[0] = f"Finding:\n{json.dumps(public, indent=1)}"


def _build_verify_views(
    finding: dict[str, Any],
    *,
    scope: Scope,
    by_path: dict[str, FileDiff],
    git: Git | None,
    mode: str,
    conventions: dict[str, list[str]],
    repo_at_head: Any,
) -> tuple[list[str], str, int | None]:
    from review_shared import build_verify_context

    source = scope.read_file(finding["file"])
    window = source_window(source, finding["line"], line_limit=RECHECK_LINE_CHARS)
    hunk = hunk_for(by_path.get(finding["file"]), finding["line"])
    hunk_text = "\n".join(render_hunk(hunk)) if hunk else "(no hunk)"
    related = related_code(git, scope, finding, source) if git is not None else ""
    if related:
        finding["_related_code"] = related

    conv_blocks = _verify_conventions_blocks(finding, mode, conventions)
    cited = (
        deep.cited_windows(repo_at_head, finding)
        if repo_at_head is not None and finding.get("citations")
        else ""
    )
    extra_views, test_window = _collect_extra_verify_views(finding, cited, scope)

    public, views, source_text = build_verify_context(
        finding,
        window=window,
        hunk_text=hunk_text,
        related=related,
        public_keys=("file", "line", "category", "claim", "why", "scenario", "suggested_fix"),
        severity_override=finding.get("finder_level") or finding["level"],
        conventions_blocks=conv_blocks,
        extra_views=extra_views,
        extra_sources=[part for part in (cited, test_window) if part],
    )
    _attach_public_rule_fields(finding, public, views)
    return (views, source_text, len(source) if source else None)


def _is_solo_verify(finding: dict[str, Any], views: list[str], verify_batch_size: int) -> bool:
    if verify_batch_size <= 1:
        return True
    if finding.get("test_evidence") or finding.get("test_run"):
        return True
    return sum(len(view) for view in views) > VERIFY_SOLO_CHARS


def _build_batch_job(batch: list[tuple[int, list[str]]]) -> tuple[str, str, list[str], list[int]]:
    if len(batch) == 1:
        return ("verifier", AUTOREVIEW_VERIFIER_TASK, batch[0][1], [batch[0][0]])

    views: list[str] = []
    for number, (_index, item_views) in enumerate(batch, 1):
        views.append(f"=== Finding {number} of {len(batch)} ===\n{item_views[0]}")
        views.extend(item_views[1:])
    return (
        "verifier",
        AUTOREVIEW_VERIFIER_BATCH_TASK,
        views,
        [index for index, _item_views in batch],
    )


def _build_verify_jobs(
    entries: list[tuple[int, list[str]]],
    verify_pool: list[dict[str, Any]],
    verify_batch_size: int,
) -> list[tuple[str, str, list[str], list[int]]]:
    jobs: list[tuple[str, str, list[str], list[int]]] = []
    by_file: dict[str, list[tuple[int, list[str]]]] = {}

    for index, views in entries:
        finding = verify_pool[index]
        if _is_solo_verify(finding, views, verify_batch_size):
            jobs.append(("verifier", AUTOREVIEW_VERIFIER_TASK, views, [index]))
        else:
            by_file.setdefault(finding["file"], []).append((index, views))

    for group in by_file.values():
        for start in range(0, len(group), verify_batch_size):
            jobs.append(_build_batch_job(group[start : start + verify_batch_size]))

    return jobs


def _resolve_batch_slot(item: dict[str, Any], position: int, size: int, out: list[Any]) -> int:
    number = item.get("finding")
    if isinstance(number, int) and 1 <= number <= size and out[number - 1] is None:
        return number - 1
    return position


def _unpack_verify_batch(result: Any, size: int) -> list[Any]:
    if not isinstance(result, list):
        err = (
            result
            if isinstance(result, (Incomplete, FrameError))
            else FrameError({"error": "the batched verifier returned no array"})
        )
        return [err] * size

    out: list[Any] = [None] * size
    for position, item in enumerate(result):
        if not isinstance(item, dict):
            continue
        slot = _resolve_batch_slot(item, position, size, out)
        if 0 <= slot < size and out[slot] is None:
            out[slot] = item

    missing = FrameError({"error": "no verdict for this finding in the batch"})
    return [item if item is not None else missing for item in out]


def _collect_initial_verdicts(
    jobs: list[tuple[str, str, list[str], list[int]]], results: list[Any]
) -> dict[int, Any]:
    verdicts: dict[int, Any] = {}
    for (_label, _task, _item, ids), result in zip(jobs, results):
        if len(ids) == 1:
            verdicts[ids[0]] = result
        else:
            for index, verdict in zip(ids, _unpack_verify_batch(result, len(ids))):
                verdicts[index] = verdict
    return verdicts


def _extract_fresh_needs(verdict: Any, seen: set[str]) -> list[str]:
    if not isinstance(verdict, dict):
        return []
    is_uncertain = (
        verdict.get("verdict") == "uncertain" or verdict.get("scenario_holds") == "unknown"
    )
    if not is_uncertain:
        return []
    raw_needs = verdict.get("needs")
    if not isinstance(raw_needs, list):
        return []
    return [need for need in raw_needs if isinstance(need, str) and need.strip() not in seen]


def _collect_followup_items(
    current_items: dict[int, list[str]],
    verdicts: dict[int, Any],
    seen_needs: dict[int, set[str]],
    verify_pool: list[dict[str, Any]],
    git: Git | None,
    scope: Scope,
) -> list[tuple[int, list[str]]]:
    again: list[tuple[int, list[str]]] = []
    for index, item in current_items.items():
        seen = seen_needs.setdefault(index, set())
        fresh = _extract_fresh_needs(verdicts.get(index), seen)
        if not fresh:
            continue
        seen.update(need.strip() for need in fresh)
        code = needed_code(git, scope, fresh)
        if not code:
            continue
        verify_pool[index].setdefault("_needed_code", []).append(code)
        updated = [*item, "Code you asked for:\n" + code]
        current_items[index] = updated
        again.append((index, updated))
    return again


async def _run_followup_hops(
    current_items: dict[int, list[str]],
    verdicts: dict[int, Any],
    verify_pool: list[dict[str, Any]],
    *,
    frames_runner: Any,
    active_model: str | None,
    verify_thinking: str | None,
    verify_cutoff: float | None,
    verifier_context: Any,
    git: Git | None,
    scope: Scope,
) -> None:
    seen_needs: dict[int, set[str]] = {}
    for _hop in range(2):
        again = _collect_followup_items(
            current_items, verdicts, seen_needs, verify_pool, git, scope
        )
        if not again:
            break
        answers = await frames_runner.run(
            "verify",
            [("verifier", AUTOREVIEW_VERIFIER_TASK, item) for _index, item in again],
            contract=None,
            model=active_model,
            thinking=verify_thinking,
            cutoff=verify_cutoff,
            context=verifier_context,
            contracts=[AUTOREVIEW_VERDICT_CONTRACT] * len(again),
        )
        for (index, _item), answer in zip(again, answers):
            if isinstance(answer, dict):
                verdicts[index] = answer


async def _run_verifier_stage(
    findings: list[dict[str, Any]],
    views: list[list[str]],
    *,
    verify_pool: list[dict[str, Any]],
    verify_batch_size: int,
    frames_runner: Any,
    active_model: str | None,
    verify_thinking: str | None,
    verify_cutoff: float | None,
    verifier_context: Any,
    git: Git | None,
    scope: Scope,
) -> list[Any]:
    base = len(verify_pool)
    verify_pool.extend(findings)
    entries = [(base + offset, item) for offset, item in enumerate(views)]
    jobs = _build_verify_jobs(entries, verify_pool, verify_batch_size)
    contracts = [
        AUTOREVIEW_VERDICT_CONTRACT if len(ids) == 1 else AUTOREVIEW_VERDICT_BATCH_CONTRACT
        for _label, _task, _item, ids in jobs
    ]
    results = await frames_runner.run(
        "verify",
        [(label, task, item) for label, task, item, _ids in jobs],
        contract=None,
        model=active_model,
        thinking=verify_thinking,
        cutoff=verify_cutoff,
        context=verifier_context,
        contracts=contracts,
    )
    verdicts = _collect_initial_verdicts(jobs, results)

    if git is not None:
        current_items = {base + offset: list(item) for offset, item in enumerate(views)}
        await _run_followup_hops(
            current_items,
            verdicts,
            verify_pool,
            frames_runner=frames_runner,
            active_model=active_model,
            verify_thinking=verify_thinking,
            verify_cutoff=verify_cutoff,
            verifier_context=verifier_context,
            git=git,
            scope=scope,
        )

    return [
        verdicts.get(base + offset, FrameError({"error": "no verdict"}))
        for offset in range(len(findings))
    ]
