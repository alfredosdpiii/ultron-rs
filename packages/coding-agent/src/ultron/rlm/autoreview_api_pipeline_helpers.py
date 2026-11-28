"""Config parsing, planning, hybrid-matching, and diagnostics helpers for `autoreview_api_pipeline`."""
from __future__ import annotations

from typing import Any, Callable

import autoreview_compiled as compiled
import autoreview_deep as deep
from autoreview_api_guides import (
    DESCRIPTION_CHARS,
    SLICE_SEPARATOR,
    TITLE_CHARS,
    _bounded,
    _number,
    _thinking,
)
from autoreview_api_rules import (
    level_rank,
    nearly_identical,
)
from autoreview_api_verify import (
    DEFAULT_VERIFY_BATCH,
    MAX_VERIFY_BATCH,
)
from knowledge_api import Knowledge
from review_api import (
    CONVENTIONS_KEY,
    MIN_BUDGET_TOKENS,
    Chunk,
    FileDiff,
    Runner,
    Scope,
    _clip,
    _text,
    file_kind,
    plan_find,
    similar_claims,
    skip_reason,
)
from review_prompts import (
    DOC_SWEEP_TASK,
    MINIMAL_FINDER_TASK,
    REVIEWERS,
    Reviewer,
    autoreview_finder_task,
)


DEFAULT_CONCURRENCY = 8
MAX_CONCURRENCY = 16
DEFAULT_DEADLINE_S = 0
DEFAULT_FRAME_TIMEOUT_S = 0
UNLIMITED = 10 ** 12
FIND_DEADLINE_SHARE = 0.75
DEFAULT_RETRY_BASE_S = 2.0
DEADLINE_ERROR = "not finished before the review deadline"

DEFAULT_THINKING = "low"
MODES = ("fast", "deep", "both", "compiled", "hybrid", "auto", "minimal")
DEFAULT_MODE = "both"
DEFAULT_DEEP_THINKING = "high"
DEFAULT_PLAN_THINKING = "high"
DEFAULT_ASK_THINKING = "low"

OVERLAP_LINES = 500
MAX_PREEXISTING = 3
BRIEF_RECORD_CHARS = 24_000
DEFAULT_MODEL_CONCURRENCY = 8
MAX_MODEL_CONCURRENCY = 16


def _optional_str(spec: dict[str, Any], key: str, default: str | None = None) -> str | None:
    value = spec.get(key)
    return value if isinstance(value, str) else default


def _string_list(spec: dict[str, Any], key: str) -> list[str]:
    value = spec.get(key)
    if not isinstance(value, list):
        return []
    return [item for item in value if isinstance(item, str)]


def _parse_budget_cap(spec: dict[str, Any]) -> int | None:
    raw_budget = spec.get("budget")
    is_positive_number = (
        isinstance(raw_budget, (int, float))
        and not isinstance(raw_budget, bool)
        and raw_budget > 0
    )
    if not is_positive_number:
        return None
    return max(int(raw_budget), MIN_BUDGET_TOKENS)


def _parse_model_concurrency(spec: dict[str, Any]) -> dict[str, int]:
    raw = spec.get("modelConcurrency")
    if not isinstance(raw, dict):
        return {}
    return {
        str(name): int(_number(limit, DEFAULT_MODEL_CONCURRENCY, 1, MAX_MODEL_CONCURRENCY))
        for name, limit in raw.items()
    }


def _parse_loki_verify_model(spec: dict[str, Any], verify_model: str | None) -> str | None:
    raw = spec.get("lokiVerifyModel")
    if isinstance(raw, str) and raw:
        return raw
    return "claude-code/haiku" if verify_model is not None else None


def _parse_run_config(spec: dict[str, Any], started: float) -> dict[str, Any]:
    """Parse and normalize run options, models, thinking levels, and deadlines from `spec`."""
    cap = _parse_budget_cap(spec)
    model = _optional_str(spec, "model")
    verify_model = _optional_str(spec, "verifyModel", model)
    plan_style = (
        spec.get("planStyle")
        if spec.get("planStyle") in compiled.PLAN_STYLES
        else compiled.DEFAULT_PLAN_STYLE
    )
    plan_default_thinking = (
        compiled.DEFAULT_CELL_THINKING if plan_style == "cell" else DEFAULT_PLAN_THINKING
    )
    deadline_s = _number(spec.get("deadlineSeconds"), DEFAULT_DEADLINE_S, 0, 24 * 3600)
    mode = spec.get("mode") if spec.get("mode") in MODES else DEFAULT_MODE

    return {
        "cap": cap,
        "budget": UNLIMITED if cap is None else cap,
        "model": model,
        "verify_model": verify_model,
        "loki_verify_model": _parse_loki_verify_model(spec, verify_model),
        "finder_models": _string_list(spec, "finderModels"),
        "thinking": _thinking(spec.get("thinking"), DEFAULT_THINKING),
        "verify_thinking": _thinking(spec.get("verifyThinking"), DEFAULT_THINKING),
        "mode": mode,
        "deep_model": _optional_str(spec, "deepModel", model),
        "deep_thinking": _thinking(spec.get("deepThinking"), DEFAULT_DEEP_THINKING),
        "plan_model": _optional_str(spec, "planModel", model),
        "plan_style": plan_style,
        "plan_thinking": _thinking(spec.get("planThinking"), plan_default_thinking),
        "ask_model": _optional_str(spec, "askModel", model),
        "ask_thinking": _thinking(spec.get("askThinking"), DEFAULT_ASK_THINKING),
        "plan_cells": int(
            _number(spec.get("planCells"), compiled.DEFAULT_PLAN_CELLS, 1, compiled.MAX_PLAN_CELLS)
        ),
        "verify_candidates_cap": int(
            _number(
                spec.get("verifyCandidates"),
                compiled.DEFAULT_VERIFY_CANDIDATES,
                1,
                compiled.MAX_VERIFY_CANDIDATES,
            )
        ),
        "verify_batch_size": int(
            _number(spec.get("verifyBatch"), DEFAULT_VERIFY_BATCH, 1, MAX_VERIFY_BATCH)
        ),
        "model_concurrency": _parse_model_concurrency(spec),
        "deep_rounds": int(
            _number(spec.get("deepRounds"), deep.DEFAULT_ROUNDS, 1, deep.MAX_ROUNDS)
        ),
        "find_cutoff": started + deadline_s * FIND_DEADLINE_SHARE if deadline_s > 0 else None,
        "verify_cutoff": started + deadline_s if deadline_s > 0 else None,
    }


def _collect_knowledge_rule_lines(
    knowledge: Any, files: list[FileDiff], limit: int
) -> list[str]:
    lines: list[str] = []
    seen: set[int] = set()
    used = 0

    for item in files:
        if skip_reason(item):
            continue
        for rule in knowledge.rules_for(item.path, limit=4):
            if rule.id in seen:
                continue
            seen.add(rule.id)
            note = rule.note
            where = f"{note.path}:{note.line}" if note.line else note.path
            body = _text(note.body, 280)
            entry = f"- R{rule.id} (for {item.path}, from #{note.pull} on {where}): {body}"
            if used + len(entry) > limit or len(lines) >= 12:
                return lines
            lines.append(entry)
            used += len(entry) + 1
    return lines


def _knowledge_rules_view(
    spec: dict[str, Any], files: list[FileDiff], limit: int = 3_500
) -> str:
    """Pre-fetch construct-matched review rules (`Knowledge.rules_for`) for the changed files."""
    stored = spec.get("knowledge") if isinstance(spec.get("knowledge"), dict) else {}
    knowledge = Knowledge.open(stored.get("db"), stored.get("repo"), stored.get("rev"))
    if knowledge is None:
        return ""
    try:
        if not knowledge.has_notes():
            return ""
        lines = _collect_knowledge_rule_lines(knowledge, files, limit)
    except Exception:
        return ""
    finally:
        knowledge.close()

    if not lines:
        return ""
    header = (
        "Past human review rules on these files and structurally similar files "
        "(untrusted repository history; check whether this change repeats any of these mistakes):\n"
    )
    return header + "\n".join(lines)


def _build_test_options(spec: dict[str, Any]) -> dict[str, Any] | None:
    if spec.get("runTests") is not True:
        return None

    test_base = spec.get("base") if spec.get("repoDir") else spec.get("baseSha")
    raw_failures = _string_list(spec, "testEnvFailures")
    return {
        "base": test_base if isinstance(test_base, str) else None,
        "runs": int(_number(spec.get("testRuns"), 6, 0, 50)),
        "timeout_s": _number(spec.get("testTimeoutSeconds"), 300, 5, 3600),
        "env_dir": _optional_str(spec, "testEnv"),
        "checkout": _optional_str(spec, "testCheckout"),
        "image": _optional_str(spec, "testImage"),
        "toolchain": _string_list(spec, "testToolchain"),
        "env_kind": _optional_str(spec, "testEnvKind"),
        "env_failures": [_text(item, 200) for item in raw_failures],
    }


def _build_repo_and_brief(
    spec: dict[str, Any],
    scope: Scope,
    context: dict[str, Any],
    deep_rev: str,
    runner: Runner | None,
    not_checked: list[str],
) -> tuple[Any, Any]:
    stored = spec.get("knowledge") if isinstance(spec.get("knowledge"), dict) else {}
    knowledge = Knowledge.open(stored.get("db"), stored.get("repo"), stored.get("rev"))
    base_rev = (str(spec.get("base")) if spec.get("repoDir") else spec.get("baseSha")) or None
    try:
        repo_at = deep.Repo(scope.root, deep_rev, runner)
        if not repo_at.files():
            raise RuntimeError("the reviewed commit could not be read")
        if knowledge is not None and knowledge.has_notes():
            repo_at.knowledge = stored
        if knowledge is not None:
            repo_at.graph = stored
        brief_at = deep.build_brief(
            repo_at,
            scope.files,
            scope.read_file,
            title=_bounded(context.get("title"), TITLE_CHARS),
            description=_bounded(context.get("description"), DESCRIPTION_CHARS),
            base=base_rev,
            knowledge=knowledge,
        )
        return repo_at, brief_at
    except Exception as error:
        detail = _text(f"{type(error).__name__}: {error}", 160)
        not_checked.append(
            f"The repository map failed ({detail}); the passes ran without the retrieved context."
        )
        return None, None
    finally:
        if knowledge is not None:
            knowledge.close()


def _prepare_deep_context(
    spec: dict[str, Any],
    scope: Scope,
    context: dict[str, Any],
    test_options: dict[str, Any] | None,
    deep_rev: str,
    runner: Runner | None,
    clock: Callable[[], float],
    stages: dict[str, int],
    not_checked: list[str],
) -> dict[str, Any] | None:
    stage_began = clock()
    repo_at, brief_at = _build_repo_and_brief(spec, scope, context, deep_rev, runner, not_checked)
    stages["mapMs"] = int((clock() - stage_began) * 1000)
    if brief_at is None:
        return None

    stage_began = clock()
    try:
        started_at = deep.start_tests(
            repo_at, scope.files, test_options, brief_at, root=scope.root, rev=deep_rev, clock=clock
        )
    except Exception as error:
        detail = f"{type(error).__name__}: {_text(str(error), 120)}"
        started_at = deep.no_tests(f"tests not run: the test session failed ({detail})")
    stages["testsMs"] = int((clock() - stage_began) * 1000)

    stage_began = clock()
    try:
        retrieved_at = compiled.retrieve(repo_at, brief_at, scope.files, clock=clock)
    except Exception as error:
        retrieved_at = {
            "text": "",
            "items": 0,
            "chars": 0,
            "ms": 0,
            "registries": {},
            "present": {},
            "keys": [],
            "sections": {},
        }
        detail = _text(f"{type(error).__name__}: {error}", 160)
        not_checked.append(
            f"The retrieval of references failed ({detail}); "
            "the passes ran without the retrieved context."
        )
    retrieved_at["reference"] = dict(brief_at.reference_stats)
    stages["retrievalMs"] = int((clock() - stage_began) * 1000)

    try:
        shapes_at = compiled.shape_items(brief_at, scope.files)
    except Exception:
        shapes_at = []

    return {
        "repo": repo_at,
        "brief": brief_at,
        "started": started_at,
        "retrieved": retrieved_at["text"],
        "retrieval": retrieved_at,
        "shapes": shapes_at,
    }


def _dedupe_compiled_findings(
    raw_findings: list[dict[str, Any]]
) -> tuple[list[dict[str, Any]], int]:
    merged: list[dict[str, Any]] = []
    duplicates = 0
    for finding in sorted(raw_findings, key=level_rank):
        twin = next(
            (
                item
                for item in merged
                if item["file"] == finding["file"]
                and item["line"] == finding["line"]
                and similar_claims(item["claim"], finding["claim"])
            ),
            None,
        )
        if twin is None:
            merged.append(finding)
        else:
            duplicates += 1
    for number, finding in enumerate(merged, 1):
        finding["id"] = number
    return merged, duplicates


def _append_compiled_step_notes(compiled_out: dict[str, Any], not_checked: list[str]) -> None:
    test_note = compiled_out["tests"]["note"]
    if test_note:
        not_checked.append(test_note[0].upper() + test_note[1:] + ".")
    if compiled_out["dropped"]:
        dropped = compiled_out["dropped"]
        not_checked.append(
            f"{len(dropped)} program finding(s) were dropped because their evidence "
            "did not check out: " + "; ".join(dropped[:4])
        )
    failed_steps = [
        record["id"] for record in compiled_out["records"] if record["status"] == "failed"
    ]
    if failed_steps:
        suffix = " ..." if len(failed_steps) > 6 else ""
        not_checked.append(
            f"{len(failed_steps)} program step(s) failed: " + ", ".join(failed_steps[:6]) + suffix
        )


def _append_compiled_notes(compiled_out: dict[str, Any], not_checked: list[str]) -> None:
    _append_compiled_step_notes(compiled_out, not_checked)
    if compiled_out["stats"]["truncated"]:
        truncated = "; ".join(compiled_out["stats"]["truncated"][:3])
        not_checked.append(f"The review program was cut at its limits: {truncated}")
    if compiled_out["uncovered"]:
        uncovered = compiled_out["uncovered"]
        suffix = " ..." if len(uncovered) > 6 else ""
        not_checked.append(
            "The review program left uncovered: " + "; ".join(uncovered[:6]) + suffix + "."
        )
    raw_notes = compiled_out.get("notes")
    notes = raw_notes if isinstance(raw_notes, list) else ()
    for note in notes:
        not_checked.append(note[0].upper() + note[1:] + ".")
    if compiled_out["diff_cut"]:
        not_checked.append("The planner saw the first part of a large diff only.")


def _collect_compiled_findings(
    compiled_out: dict[str, Any], not_checked: list[str]
) -> tuple[
    list[dict[str, Any]],
    int,
    list[str],
    int,
    dict[str, Any],
    list[str],
    dict[str, Any],
    list[dict[str, Any]],
]:
    merged, duplicates = _dedupe_compiled_findings(compiled_out["findings"])
    _append_compiled_notes(compiled_out, not_checked)
    return (
        merged,
        duplicates,
        compiled_out["generic"],
        compiled_out["refuted"],
        compiled_out["tests"],
        compiled_out["assurance"],
        compiled_out["stats"],
        compiled_out["records"],
    )


def _format_session_runs(session: Any) -> list[dict[str, Any]]:
    if session is None:
        return []
    return [
        {key: value for key, value in record.items() if key != "output"}
        | {"output": record["output"][-600:]}
        for record in session.records
    ]


def _format_minimal_conventions_view(conventions: dict[str, list[str]]) -> str:
    seen: list[str] = []
    for entries in conventions.values():
        for entry in entries:
            if entry not in seen:
                seen.append(entry)
    if not seen:
        return ""
    return _clip(
        "Repository conventions (base CLAUDE.md / AGENTS.md):\n" + "\n\n".join(seen),
        6_000,
    )


def _is_doc_only_chunk(chunk: Chunk, members: dict[int, list[Chunk]]) -> bool:
    return all(file_kind(member.path) == "doc" for member in members.get(chunk.id, [chunk]))


def _build_doc_sweep_entry(
    doc_chunks: list[Chunk], members: dict[int, list[Chunk]]
) -> tuple[tuple[Reviewer, Chunk], tuple[str, str, Any]]:
    doc_reviewer = Reviewer("doc-sweep", "Doc sweep", "", ())
    doc_text = SLICE_SEPARATOR.join(chunk.text for chunk in doc_chunks)[:40_000]
    doc_bundle = Chunk(-1, doc_chunks[0].path, "doc", "modified", [], doc_text)
    members[-1] = [member for chunk in doc_chunks for member in members.get(chunk.id, [chunk])]
    return (doc_reviewer, doc_bundle), ("doc-sweep", DOC_SWEEP_TASK, doc_text)


def _build_minimal_fast_plan(
    chunks: list[Chunk], members: dict[int, list[Chunk]]
) -> tuple[list[tuple[Reviewer, Chunk]], list[tuple[str, str, Any]]]:
    minimal_reviewer = Reviewer("minimal", "Minimal review", "", ())
    code_chunks = [chunk for chunk in chunks if not _is_doc_only_chunk(chunk, members)]
    doc_chunks = [chunk for chunk in chunks if _is_doc_only_chunk(chunk, members)]
    frames: list[tuple[Reviewer, Chunk]] = [(minimal_reviewer, chunk) for chunk in code_chunks]
    tasks: list[tuple[str, str, Any]] = [
        ("minimal", MINIMAL_FINDER_TASK, chunk.text) for _, chunk in frames
    ]
    if doc_chunks:
        frame_entry, task_entry = _build_doc_sweep_entry(doc_chunks, members)
        frames.append(frame_entry)
        tasks.append(task_entry)
    return frames, tasks


def _record_find_plan_notes(
    plan: dict[str, Any], not_checked: list[str], incomplete: list[str]
) -> None:
    for key, count in plan["not_applicable"].items():
        reviewer = REVIEWERS[key]
        why = "no LLM-related code" if reviewer.llm_only else "documentation only"
        not_checked.append(f"{reviewer.title} reviewer skipped {count} slice(s) with {why}.")
    if plan["dropped"]:
        paths = sorted({chunk.path for _, chunk in plan["dropped"]})
        suffix = " ..." if len(paths) > 10 else ""
        incomplete.append(
            f"{len(plan['dropped'])} reviewer passes did not fit the token budget: "
            + ", ".join(paths[:10])
            + suffix
        )


def _build_reviewer_task(reviewer: Reviewer, chunk: Chunk) -> tuple[str, str, Any]:
    if reviewer.key == CONVENTIONS_KEY and reviewer.checklist:
        slim = Reviewer(
            reviewer.key, reviewer.title, reviewer.focus, (), only_paths=reviewer.only_paths
        )
        conv_view = "Repository and directory convention files:\n" + "\n\n".join(reviewer.checklist)
        return (reviewer.key, autoreview_finder_task(slim), [conv_view, chunk.text])
    return (reviewer.key, autoreview_finder_task(reviewer), chunk.text)


def _build_standard_fast_plan(
    chunks: list[Chunk],
    reviewers: list[Reviewer],
    find_budget: int,
    frames_context: list[str] | None,
    not_checked: list[str],
    incomplete: list[str],
) -> tuple[list[tuple[Reviewer, Chunk]], list[tuple[str, str, Any]]]:
    context_parts = frames_context if frames_context is not None else ()
    overhead = sum(len(part) for part in context_parts) // 3
    plan = plan_find(
        chunks, reviewers, max(0, find_budget - overhead * len(chunks) * len(reviewers))
    )
    _record_find_plan_notes(plan, not_checked, incomplete)
    frames: list[tuple[Reviewer, Chunk]] = plan["frames"]
    tasks = [_build_reviewer_task(reviewer, chunk) for reviewer, chunk in frames]
    return frames, tasks


def _is_early_verify_duplicate(finding: dict[str, Any], pool: list[dict[str, Any]]) -> bool:
    for other in pool:
        if other["file"] != finding["file"] or other["category"] != finding["category"]:
            continue
        close = abs(other["line"] - finding["line"]) <= deep.MERGE_WINDOW
        if close or nearly_identical(other["claim"], finding["claim"]):
            return True
    return False


_HYBRID_INHERITED_KEYS = (
    "verification",
    "level",
    "severity",
    "evidence",
    "verified_by",
    "host_confirmed",
    "how_verified",
    "test_run",
    "test_evidence",
    "ask_evidence",
    "proof",
    "beyond_diff",
)


def _find_hybrid_twin(
    finding: dict[str, Any], before_merge: list[dict[str, Any]]
) -> dict[str, Any] | None:
    for item in before_merge:
        if not item.get("verification") or item["file"] != finding["file"]:
            continue
        if abs(item["line"] - finding["line"]) > deep.DEDUPE_WINDOW:
            continue
        if similar_claims(item["claim"], finding["claim"]) or item["category"] == finding["category"]:
            return item
    return None


def _inherit_hybrid_verifications(
    merged: list[dict[str, Any]],
    deep_findings: list[dict[str, Any]],
    before_merge: list[dict[str, Any]],
) -> None:
    for finding in merged:
        if finding.get("verification") or finding not in deep_findings:
            continue
        twin = _find_hybrid_twin(finding, before_merge)
        if twin is None or twin["verification"]["state"] not in ("confirmed", "refuted"):
            continue
        for key in _HYBRID_INHERITED_KEYS:
            if twin.get(key) is not None:
                finding[key] = twin[key]
        finding["verification"] = dict(
            finding["verification"], inherited_from=twin.get("source")
        )


def _record_investigator_gaps(
    investigators: list[dict[str, Any]], not_checked: list[str]
) -> None:
    lax = [record for record in investigators if record.get("untraced")]
    if lax:
        details = "; ".join(
            f"{', '.join(record['untraced'][:4])} ({record['lens']})" for record in lax
        )
        not_checked.append(f"Not traced by the deep pass: {details}.")

    owed = [record for record in investigators if record.get("unchecked")]
    if owed:
        details = "; ".join(
            f"{', '.join(record['unchecked'][:4])} ({record['lens']})" for record in owed
        )
        not_checked.append(f"Checks the deep pass neither made nor declared: {details}.")


def _record_failed_investigators(
    investigators: list[dict[str, Any]],
    run_fast: bool,
    not_checked: list[str],
    incomplete: list[str],
) -> None:
    failed = [record for record in investigators if record["status"] == "failed"]
    if not failed:
        return
    cut = any(record.get("error") == DEADLINE_ERROR for record in failed)
    target = incomplete if cut or not run_fast else not_checked
    summary = ", ".join(f"{record['lens']} ({record.get('error', '')})" for record in failed)
    target.append(f"{len(failed)} investigator(s) of the deep pass failed: {summary}")


def _record_deep_diagnostics(
    deep_out: dict[str, Any],
    investigators: list[dict[str, Any]],
    mode: str,
    run_fast: bool,
    not_checked: list[str],
    incomplete: list[str],
) -> None:
    _record_failed_investigators(investigators, run_fast, not_checked, incomplete)
    if deep_out["dropped"]:
        dropped = "; ".join(deep_out["dropped"][:4])
        not_checked.append(
            f"{len(deep_out['dropped'])} deep finding(s) were dropped because their evidence "
            f"did not check out: {dropped}"
        )

    _record_investigator_gaps(investigators, not_checked)

    if deep_out["diff_cut"]:
        not_checked.append("The investigators saw the first part of a large diff only.")
    if mode == "deep" and not deep_out["lenses"]:
        incomplete.append("no investigator applied to this change")


def _resolve_final_mode(
    mode: str, compiled_out: dict[str, Any] | None, hybrid: bool, deep_out: dict[str, Any] | None
) -> str:
    if compiled_out is not None:
        return "compiled"
    if hybrid:
        return "hybrid"
    if mode == "minimal":
        return "minimal"
    if deep_out is not None or mode == "fast":
        return "both" if mode == "hybrid" else mode
    return "fast"


