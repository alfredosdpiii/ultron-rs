"""Pipeline configuration, deep/compiled/fast/sweep/verify stage orchestration for autoreview."""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
from typing import Any, Callable

import autoreview_blind as blind
import autoreview_compiled as compiled
import autoreview_deep as deep
import autoreview_static as static_checks
from autoreview_api_guides import (
    DESCRIPTION_CHARS,
    SINGLE_SLICE_LINES,
    SLICE_SEPARATOR,
    TITLE_CHARS,
    _bounded,
    _number,
    _pick_auto_mode,
    _read_text,
    _thinking,
    context_block,
    diff_line_ranges,
    load_guides,
    member_for,
    merged_reviewers,
    pack_chunks,
    raised_by_others,
    repo_review_guide,
    stated_intent,
)
from autoreview_api_recheck import RECHECK_LINE_CHARS, _run_recheck_stage
from autoreview_api_rules import (
    AUTOREVIEW_FINDINGS_CONTRACT,
    LEVELS,
    _extras,
    _public,
    apply_lessons,
    capped_level,
    dedupe_fast,
    final_level,
    generic_reason,
    group_root_causes,
    level_rank,
    nearly_identical,
    parse_rules,
    set_level,
    to_level,
)
from autoreview_api_verify import (
    DEFAULT_VERIFY_BATCH,
    EARLIER_WINDOW,
    MAX_VERIFY_BATCH,
    _build_verify_views,
    _filter_verify_candidates,
    _run_verifier_stage,
    _summarize_hybrid_verification,
)
from infer_api import FrameError, Incomplete
from knowledge_api import Knowledge
from review_api import (
    CONVENTIONS_KEY,
    FIND_SHARE,
    MAX_FINDINGS_PER_FRAME,
    MIN_BUDGET_TOKENS,
    SWEEP_DIFF_CHARS,
    Chunk,
    FileDiff,
    Git,
    ReviewError,
    Runner,
    Scope,
    _clip,
    _estimate_tokens,
    _rev_reader,
    _text,
    apply_verdicts,
    build_chunks,
    conventions_inventory,
    conventions_reviewer,
    file_kind,
    hunk_for,
    normalize_findings,
    out_of_diff_callers,
    parse_diff,
    plan_find,
    recall_memorantado,
    render_hunk,
    similar_claims,
    skip_reason,
    source_window,
)
from review_prompts import (
    AUTOREVIEW_SWEEP_TASK,
    AUTOREVIEW_VERIFIER_TASK,
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
            entry = f"- R{rule.id} (for {item.path}, from #{note.pull} on {where}): {_text(note.body, 280)}"
            if used + len(entry) > limit or len(lines) >= 12:
                return lines
            lines.append(entry)
            used += len(entry) + 1
    return lines


def _knowledge_rules_view(
    spec: dict[str, Any], files: list[FileDiff], limit: int = 3_500
) -> str:
    """Pre-fetch top construct-matched human review rules (`Knowledge.rules_for`) from the knowledge graph for the
    changed files, so `minimal` and `fast` finders get past reviewer patterns automatically."""
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


class ReviewPipeline:
    """Stateful orchestrator for one `autoreview_api.run` execution."""

    def __init__(
        self,
        rlm: Any,
        spec: dict[str, Any],
        *,
        runner: Runner | None,
        clock: Callable[[], float],
        sleep: Callable[[float], Any],
        rng: Callable[[], float],
        scope_fn: Callable[[dict[str, Any], Runner | None], tuple[Scope, Git | None, list[str]]],
        reviewer_keys_fn: Callable[[Any], list[str]],
        recipe_fn: Callable[[str | None, str], str],
        frames_cls: Any,
        usage_cls: Any,
        describe_failure_fn: Callable[[Any], str | None],
        is_throttled_fn: Callable[[Any], bool],
    ) -> None:
        self.rlm = rlm
        self.spec = spec
        self.runner = runner
        self.clock = clock
        self.sleep = sleep
        self.rng = rng
        self.describe_failure_fn = describe_failure_fn
        self.is_throttled_fn = is_throttled_fn
        self.started = clock()
        self.cfg = _parse_run_config(spec, self.started)
        self.mode = self.cfg["mode"]
        os.environ.setdefault("GIT_NO_LAZY_FETCH", "1")
        self.reviewers = [REVIEWERS[key] for key in reviewer_keys_fn(spec.get("only"))]
        self.scope, self.git, self.incomplete = scope_fn(spec, runner)
        self.usage = usage_cls()
        self.frames_runner = frames_cls(
            rlm,
            cap=self.cfg["cap"],
            usage=self.usage,
            concurrency=int(_number(spec.get("concurrency"), DEFAULT_CONCURRENCY, 1, MAX_CONCURRENCY)),
            frame_timeout_s=_number(
                spec.get("frameTimeoutSeconds"), DEFAULT_FRAME_TIMEOUT_S, 0, 3600
            ),
            retry_base_s=_number(spec.get("retryBaseSeconds"), DEFAULT_RETRY_BASE_S, 0, 60),
            clock=clock,
            sleep=sleep,
            rng=rng,
            model_concurrency=self.cfg["model_concurrency"],
            system_prefix=spec.get("systemPrefix") is True,
        )
        self.recipe_fn = recipe_fn
        self.not_checked: list[str] = []
        self.stages: dict[str, int] = {}
        self.context = spec.get("context") if isinstance(spec.get("context"), dict) else {}
        raw_comments = self.context.get("comments")
        self.others = (
            [item for item in raw_comments if isinstance(item, dict)]
            if isinstance(raw_comments, list)
            else []
        )
        self.base_rev = spec.get("base") if spec.get("repoDir") else spec.get("baseSha")
        self.deep_rev = (
            (str(spec.get("head")) if spec.get("repoDir") else "HEAD")
            if self.git is not None
            else None
        )
        self.by_path = {item.path: item for item in self.scope.files}

        self.shared = ""
        self.guidance = ""
        self.guide_names: list[str] = []
        self.stated = ""
        self.post_files: list[FileDiff] = self.scope.files
        self.chunks: list[Chunk] = []
        self.members: dict[int, list[Chunk]] = {}
        self.changed_lines = 0
        self.conventions: dict[str, list[str]] = {}
        self.scope_ms = 0
        self.recheck_ms = 0
        self.find_ms = 0
        self.deep_ms = 0
        self.program_ms = 0
        self.verify_ms = 0
        self.find_started = 0.0
        self.find_budget = 0

        self.earlier: list[dict[str, Any]] = []
        self.open_earlier: list[dict[str, Any]] = []
        self.raised: list[dict[str, Any]] = []
        self.dropped_generic: list[str] = []
        self.duplicate_records: list[dict[str, Any]] = []
        self.merged: list[dict[str, Any]] = []
        self.duplicates = 0
        self.refuted = 0
        self.refuted_leads: list[dict[str, Any]] = []
        self.test_options = _build_test_options(spec)
        self.test_report: dict[str, Any] = {
            "enabled": False,
            "mechanism": None,
            "note": None,
            "runs": [],
            "env": "none",
            "toolchain": [],
        }
        self.compiled_out: dict[str, Any] | None = None
        self.program_stats: dict[str, Any] | None = None
        self.static_out: dict[str, Any] | None = None
        self.prepared: dict[str, Any] | None = None
        self.minimal_started: dict[str, Any] | None = None
        self.hybrid = False
        self.run_fast = False
        self.run_deep_pass = False
        self.parallel = False
        self.diff_text_all = ""
        self.reference_audience = "investigators"
        self.frames_context: list[str] | None = None
        self.verifier_context: list[str] | None = None
        self.early_tasks: list[Any] = []
        self.verify_estimate = 0
        self.verify_pool: list[dict[str, Any]] = []
        self.deep_out: dict[str, Any] | None = None
        self.investigators: list[dict[str, Any]] = []
        self.assurance: list[str] = []
        self.program_records: list[dict[str, Any]] = []
        self.verification: dict[str, Any] | None = None
        self.verify_batches: list[dict[str, Any]] = []
        self.found_static: list[dict[str, Any]] = []
        self.also_raised: list[dict[str, Any]] = []
        self.contradicted: list[dict[str, Any]] = []
        self.confirmed: list[dict[str, Any]] = []
        self.uncertain: list[dict[str, Any]] = []
        self.rejected: list[dict[str, Any]] = []
        self.blind_out: dict[str, Any] | None = None

    def _prepare_chunks_and_reviewers(self) -> None:
        post_text = _read_text(self.spec.get("postDiffPath"))
        self.post_files = parse_diff(post_text) if post_text else self.scope.files
        file_chunks, skipped = build_chunks(self.scope.files, self.scope.read_file)
        self.changed_lines = sum(
            item.added + item.removed for item in self.scope.files if not skip_reason(item)
        )
        self.chunks, self.members = pack_chunks(
            file_chunks,
            [item.path for item in self.scope.files],
            single=self.changed_lines < SINGLE_SLICE_LINES,
        )
        self.mode = self.recipe_fn(self.cfg["model"], self.mode)
        self.reviewers = merged_reviewers(self.reviewers, self.changed_lines)
        self._load_conventions()
        self.not_checked += [f"{path}: {reason}" for path, reason in skipped]
        self._record_empty_chunks()

    def _prepare_context_and_chunks(self) -> None:
        self.shared = context_block(self.context, self.scope.read_file)
        repo_label = str(self.spec.get("repo") or self.spec.get("label") or "")
        guidance, self.guide_names = load_guides(
            self.spec.get("guides"), repo_label, self.scope.files
        )
        repo_guide, repo_guide_note = repo_review_guide(self.scope, self.git, self.base_rev)
        self.guidance = "\n\n".join(part for part in (guidance, repo_guide) if part)
        if repo_guide_note:
            self.not_checked.append(repo_guide_note)
        self.stated = stated_intent(self.context, self.scope, self.git, self.spec)
        self._prepare_chunks_and_reviewers()
        self.scope_ms = int((self.clock() - self.started) * 1000)

    def _load_conventions(self) -> None:
        if not self.chunks or self.spec.get("only"):
            return
        base_str = self.base_rev if isinstance(self.base_rev, str) else None
        self.conventions, conv_notes = conventions_inventory(
            self.git,
            self.scope,
            base=base_str,
            names=("CLAUDE.md", "CLAUDE.local.md", "AGENTS.md"),
        )
        self.not_checked += conv_notes
        if not self.conventions or self.mode == "minimal":
            return

        conv_slices = tuple(
            chunk.path
            for chunk in self.chunks
            if any(member.path in self.conventions for member in self.members[chunk.id])
        )
        if conv_slices:
            conv_rev = conventions_reviewer(self.conventions, only_paths=conv_slices)
            self.reviewers = [*self.reviewers, conv_rev]

    def _record_empty_chunks(self) -> None:
        raw_unapproved = self.spec.get("unapprovedSources")
        unapproved = [str(item) for item in raw_unapproved] if isinstance(raw_unapproved, list) else []
        if not self.chunks and unapproved:
            verb = "was" if len(unapproved) == 1 else "were"
            self.incomplete.append(
                f"every changed line was carried from {', '.join(unapproved)}, which {verb} not approved"
            )
        elif not self.chunks and not self.spec.get("carried") and self.spec.get("recheckOnly") is not True:
            msg = "the diff has no reviewable changes" if self.scope.files else "the diff is empty"
            self.incomplete.append(msg)

    async def _run_compiled_pass(self) -> None:
        program_started = self.clock()
        if self.mode == "auto":
            self.mode = _pick_auto_mode(self.chunks)
        if self.mode == "compiled":
            if self.deep_rev is None:
                self.not_checked.append("The compiled mode was skipped: the repository was not available.")
                self.mode = "fast"
            elif self.chunks:
                await self._execute_compiled_mode()

        if self.mode == "compiled" or self.program_stats:
            self.program_ms = int((self.clock() - program_started) * 1000)
        if self.mode in ("deep", "both", "hybrid") and self.deep_rev is None:
            self.not_checked.append("The deep pass was skipped: the repository was not available.")

    def _load_compiled_program(self) -> Any:
        program_path = self.spec.get("programPath")
        if not isinstance(program_path, str) or not program_path:
            return None
        try:
            return json.loads(Path(program_path).read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            raise ReviewError(f"the program file could not be read: {error}") from None

    def _apply_compiled_fallback(self) -> None:
        if self.compiled_out is None or not self.compiled_out["fallback"]:
            return
        fallback = self.compiled_out["fallback"]
        self.not_checked.append(
            f"The compiled mode fell back to the fast and deep passes: {fallback}."
        )
        self.program_stats = {"planner": self.compiled_out["planner"], "fallback": fallback}
        self.compiled_out = None
        self.mode = "both"

    async def _execute_compiled_mode(self) -> None:
        program = self._load_compiled_program()
        stored = self.spec.get("knowledge") if isinstance(self.spec.get("knowledge"), dict) else {}
        knowledge = Knowledge.open(stored.get("db"), stored.get("repo"), stored.get("rev"))
        base_str = (
            str(self.spec.get("base")) if self.spec.get("repoDir") else self.spec.get("baseSha")
        ) or None
        try:
            self.compiled_out = await compiled.run_compiled(
                self.frames_runner,
                self.scope.files,
                self.scope.read_file,
                root=self.scope.root,
                rev=self.deep_rev,
                diff_text=SLICE_SEPARATOR.join(chunk.text for chunk in self.chunks),
                context=self.shared,
                intent=self.stated,
                guidance=self.guidance,
                plan_model=self.cfg["plan_model"],
                plan_thinking=self.cfg["plan_thinking"],
                ask_model=self.cfg["ask_model"],
                ask_thinking=self.cfg["ask_thinking"],
                cutoff=self.cfg["find_cutoff"],
                clock=self.clock,
                cap=capped_level,
                runner=self.runner,
                tests=self.test_options,
                to_level=to_level,
                title=_bounded(self.context.get("title"), TITLE_CHARS),
                description=_bounded(self.context.get("description"), DESCRIPTION_CHARS),
                base=base_str,
                enrich=parse_rules,
                generic=generic_reason,
                program=program,
                dump_path=_optional_str(self.spec, "dumpProgramPath"),
                plan_style=self.cfg["plan_style"],
                plan_cells=self.cfg["plan_cells"],
                python=_optional_str(self.spec, "cellPython", "python3") or "python3",
                knowledge=knowledge,
            )
        except Exception as error:
            detail = _text(f"{type(error).__name__}: {error}", 160)
            self.not_checked.append(
                f"The review program failed ({detail}); the fast and deep passes ran instead."
            )
            self.mode = "both"
        finally:
            if knowledge is not None:
                knowledge.close()
        self._apply_compiled_fallback()

    async def _run_static_stage(self) -> None:
        if not self.chunks:
            return
        read_base = (
            _rev_reader(self.git, self.base_rev)
            if self.git is not None and isinstance(self.base_rev, str) and self.base_rev
            else lambda _path: None
        )
        try:
            self.static_out = await static_checks.run_static(
                self.rlm, self.scope.files, self.scope.read_file, read_base
            )
            self.not_checked += static_checks.notes(self.static_out)
        except Exception as error:
            detail = _text(f"{type(error).__name__}: {error}", 160)
            self.not_checked.append(f"The deterministic stage failed ({detail}).")

    def _run_minimal_tests(self) -> None:
        stage_began = self.clock()
        try:
            repo_min = deep.Repo(self.scope.root, self.deep_rev, self.runner)
            test_files = [
                item.path
                for item in self.scope.files
                if not skip_reason(item) and deep.file_kind(item.path) == "test"
            ]
            brief_min = deep.Brief(text="", lenses=[], tests=test_files)
            self.minimal_started = deep.start_tests(
                repo_min,
                self.scope.files,
                self.test_options,
                brief_min,
                root=self.scope.root,
                rev=self.deep_rev,
                clock=self.clock,
            )
            session = self.minimal_started.get("session")
            raw_toolchain = self.minimal_started.get("toolchain")
            toolchain = list(raw_toolchain) if isinstance(raw_toolchain, list) else []
            self.test_report = {
                "enabled": True,
                "mechanism": self.minimal_started.get("mechanism"),
                "note": self.minimal_started.get("note"),
                "runs": _format_session_runs(session),
                "env": self.minimal_started.get("env") or "none",
                "toolchain": toolchain,
            }
            if self.test_report["note"]:
                note = self.test_report["note"]
                self.not_checked.append(note[0].upper() + note[1:] + ".")
            if session is not None:
                session.close()
        except Exception as error:
            detail = _text(f"{type(error).__name__}: {error}", 160)
            self.not_checked.append(f"Sandboxed tests did not run ({detail}).")
        self.stages["testsMs"] = int((self.clock() - stage_began) * 1000)

    def _resolve_pass_flags(self) -> None:
        self.hybrid = self.mode == "hybrid" and self.prepared is not None
        if self.mode == "hybrid" and not self.hybrid:
            if self.deep_rev is not None and self.chunks:
                self.not_checked.append(
                    "The hybrid mode's map failed; the fast and deep passes ran instead."
                )
            self.mode = "both"

        self.run_fast = self.mode in ("fast", "both", "hybrid", "minimal") or (
            self.mode == "deep" and self.deep_rev is None
        )
        self.run_deep_pass = self.mode in ("deep", "both", "hybrid") and self.deep_rev is not None

    def _prepare_deep_or_minimal_tests(self) -> None:
        has_repo_chunks = self.deep_rev is not None and bool(self.chunks)
        if has_repo_chunks and self.mode in ("deep", "both", "hybrid"):
            self.prepared = _prepare_deep_context(
                self.spec,
                self.scope,
                self.context,
                self.test_options,
                self.deep_rev,
                self.runner,
                self.clock,
                self.stages,
                self.not_checked,
            )
        elif has_repo_chunks and self.mode == "minimal" and self.test_options is not None:
            self._run_minimal_tests()
        self._resolve_pass_flags()

    async def _collect_auxiliary_views(self) -> tuple[str, str, str, str, str]:
        raw_audience = self.spec.get("referenceView")
        self.reference_audience = (
            raw_audience if raw_audience in ("investigators", "all", "none") else "investigators"
        )
        reference_text = self.prepared["brief"].reference_text if self.prepared else ""
        reference_view = reference_text if self.reference_audience == "all" else ""
        rules_view = _knowledge_rules_view(self.spec, self.scope.files) if self.chunks else ""
        memory_label = str(self.context.get("title") or self.scope.label)
        memory_view = (
            await recall_memorantado(self.rlm, self.scope.files, label=memory_label)
            if self.chunks
            else ""
        )
        minimal_conv = (
            _format_minimal_conventions_view(self.conventions)
            if self.mode == "minimal" and self.conventions
            else ""
        )
        minimal_tests = deep.tests_block(self.minimal_started) if self.minimal_started else ""
        return reference_view, rules_view, memory_view, minimal_conv, minimal_tests

    def _retrieved_context_view(self) -> str:
        if not self.prepared or not self.prepared.get("retrieved"):
            return ""
        return (
            "Retrieved context, looked up by the host at the reviewed commit "
            f"(untrusted repository data):\n{self.prepared['retrieved']}"
        )

    def _select_prefix_or_views(
        self, system_prefix: bool, shared_views: list[str], fallback_views: list[str]
    ) -> list[str] | None:
        chosen = shared_views if system_prefix else fallback_views
        return chosen if chosen else None

    async def _build_frame_contexts(self) -> None:
        self.diff_text_all = SLICE_SEPARATOR.join(chunk.text for chunk in self.chunks)
        system_prefix = self.spec.get("systemPrefix") is True
        (
            reference_view,
            rules_view,
            memory_view,
            minimal_conv,
            minimal_tests,
        ) = await self._collect_auxiliary_views()

        brief_block = self._build_shared_brief_block(minimal_tests)
        retrieved_raw = self.prepared["retrieved"] if self.prepared else ""
        shared_views = deep.shared_views(
            self.diff_text_all,
            brief_block,
            retrieved_raw,
            self.shared,
            self.stated,
            self.guidance,
            reference=reference_view,
        )
        retrieved_view = self._retrieved_context_view()
        raw_finder_parts = (
            self.guidance,
            self.shared,
            self.stated,
            minimal_conv,
            rules_view,
            memory_view,
            minimal_tests,
            retrieved_view,
            reference_view,
        )
        finder_views = [part for part in raw_finder_parts if part]
        verifier_views = [part for part in (self.stated, self.guidance) if part]
        self.frames_context = self._select_prefix_or_views(system_prefix, shared_views, finder_views)
        self.verifier_context = self._select_prefix_or_views(
            system_prefix, shared_views, verifier_views
        )

    def _build_shared_brief_block(self, minimal_tests_view: str) -> str | None:
        if not self.prepared:
            return minimal_tests_view or None
        tests_text = deep.tests_block(self.prepared["started"])
        if tests_text:
            return f"{self.prepared['brief'].text}\n\n{tests_text}"
        return self.prepared["brief"].text

    def _ingest_fast_frame_items(
        self, reviewer: Reviewer, chunk: Chunk, result: Any
    ) -> int:
        cap_per_frame = 4 if reviewer.key == "doc-sweep" else MAX_FINDINGS_PER_FRAME
        items = result[:cap_per_frame] if isinstance(result, list) else []
        unattributed = 0
        for item in items:
            member = member_for(self.members[chunk.id], item)
            if member is None:
                unattributed += 1
                continue
            source = self.scope.read_file(member.path)
            normalized = normalize_findings(
                [item], reviewer, member, len(source) if source else None
            )
            for finding in normalized:
                _extras(item, finding)
                self.raised.append(finding)
        return unattributed

    def _collect_fast_results(
        self, frames: list[tuple[Reviewer, Chunk]], results: list[Any]
    ) -> None:
        failures: dict[str, list[str]] = {}
        unattributed = 0
        for (reviewer, chunk), result in zip(frames, results):
            failure = self.describe_failure_fn(result)
            if failure:
                failures.setdefault(failure, []).append(f"{reviewer.key} on {chunk.path}")
                continue
            unattributed += self._ingest_fast_frame_items(reviewer, chunk, result)

        for failure, where in failures.items():
            suffix = " ..." if len(where) > 8 else ""
            self.incomplete.append(
                f"{len(where)} reviewer passes {failure}: " + ", ".join(where[:8]) + suffix
            )
        if unattributed:
            self.not_checked.append(
                f"{unattributed} finding(s) named no file of their slice and were dropped."
            )

    def _build_fast_plan(
        self,
    ) -> tuple[list[tuple[Reviewer, Chunk]], list[tuple[str, str, Any]]]:
        if self.mode == "minimal":
            return _build_minimal_fast_plan(self.chunks, self.members)
        return _build_standard_fast_plan(
            self.chunks,
            self.reviewers,
            self.find_budget,
            self.frames_context,
            self.not_checked,
            self.incomplete,
        )

    async def _run_fast_frames(
        self, frames: list[tuple[Reviewer, Chunk]], tasks_to_run: list[tuple[str, str, Any]]
    ) -> None:
        finder_models = self.cfg["finder_models"]
        models = (
            [finder_models[idx % len(finder_models)] for idx in range(len(frames))]
            if finder_models
            else None
        )
        results = await self.frames_runner.run(
            "find",
            tasks_to_run,
            contract=AUTOREVIEW_FINDINGS_CONTRACT,
            model=self.cfg["model"],
            thinking=self.cfg["thinking"],
            models=models,
            context=self.frames_context,
            cutoff=self.cfg["find_cutoff"],
        )
        self._collect_fast_results(frames, results)

    async def fast_pass(self) -> None:
        if not (self.chunks and self.run_fast):
            return
        try:
            frames, tasks_to_run = self._build_fast_plan()
            if frames:
                await self._run_fast_frames(frames, tasks_to_run)
        except Exception as error:
            detail = _text(f"{type(error).__name__}: {error}", 160)
            self.incomplete.append(f"the fast pass failed ({detail})")

        specific: list[dict[str, Any]] = []
        for finding in self.raised:
            finding["source"] = "fast"
            finding.setdefault("level", to_level(finding["severity"]))
            reason = generic_reason(finding)
            if reason:
                self.dropped_generic.append(reason)
            else:
                specific.append(finding)

        self.merged = dedupe_fast(specific, self.duplicate_records)
        self.duplicates = len(specific) - len(self.merged)
        self.find_ms = int((self.clock() - self.find_started) * 1000)
        self.stages["findMs"] = self.find_ms

    def verify_views(self, finding: dict[str, Any]) -> tuple[list[str], str, int | None] | None:
        return _build_verify_views(
            finding,
            scope=self.scope,
            by_path=self.by_path,
            git=self.git,
            mode=self.mode,
            conventions=self.conventions,
            repo_at_head=self._active_blind_repo(),
        )

    async def run_verifier(
        self,
        findings: list[dict[str, Any]],
        views: list[list[str]],
        *,
        model_override: str | None = None,
    ) -> list[Any]:
        active_model = self.cfg["verify_model"] if model_override is None else model_override
        return await _run_verifier_stage(
            findings,
            views,
            verify_pool=self.verify_pool,
            verify_batch_size=self.cfg["verify_batch_size"],
            frames_runner=self.frames_runner,
            active_model=active_model,
            verify_thinking=self.cfg["verify_thinking"],
            verify_cutoff=self.cfg["verify_cutoff"],
            verifier_context=self.verifier_context,
            git=self.git,
            scope=self.scope,
        )

    def skip_candidate(self, finding: dict[str, Any]) -> bool:
        if raised_by_others(finding, self.others):
            return True
        return any(
            item["file"] == finding["file"]
            and abs(item["line"] - finding["line"]) <= EARLIER_WINDOW
            and similar_claims(item["claim"], finding["claim"])
            for item in self.open_earlier
        )

    def _should_skip_early_verify(
        self, finding: dict[str, Any], pool: list[dict[str, Any]]
    ) -> bool:
        if finding.get("host_confirmed") or finding["category"] == "tests":
            return True
        if self.skip_candidate(finding) or static_checks.contradiction(finding, self.static_out):
            return True
        return _is_early_verify_duplicate(finding, pool)

    async def early_verify(self, lens: str, findings: list[dict[str, Any]]) -> None:
        _ = lens
        queued = list(self.verify_pool) + [
            item for task in self.early_tasks for item in getattr(task, "_findings", [])
        ]
        chosen: list[dict[str, Any]] = []
        views_of: list[list[str]] = []
        meta: list[tuple[str, int | None]] = []

        for finding in findings:
            if self._should_skip_early_verify(finding, queued + chosen):
                continue
            built = self.verify_views(finding)
            if built is None:
                continue
            views, source_text, count = built
            cost = _estimate_tokens(AUTOREVIEW_VERIFIER_TASK, *views, output=600)
            if self.verify_estimate + cost > max(0, self.cfg["budget"] - self.usage.tokens):
                continue
            self.verify_estimate += cost
            chosen.append(finding)
            views_of.append(views)
            meta.append((source_text, count))

        if not chosen:
            return

        async def judge() -> None:
            verdicts = await self.run_verifier(chosen, views_of)
            for finding, verdict, (source_text, count) in zip(chosen, verdicts, meta):
                needed_list = finding.get("_needed_code")
                extra = "\n\n".join(needed_list) if isinstance(needed_list, list) else ""
                full_source = f"{source_text}\n{extra}" if extra else source_text
                finding["_early"] = {"verdict": verdict, "source": full_source, "count": count}

        task = asyncio.ensure_future(judge())
        setattr(task, "_findings", chosen)
        self.early_tasks.append(task)

    async def verify_batch(
        self, batch: str, items: list[dict[str, Any]], start: int, limit: int
    ) -> dict[str, Any] | None:
        assert self.prepared is not None
        diff_lines = {
            item.path: {line.new for hunk in item.hunks for line in hunk.lines if line.new is not None}
            for item in self.scope.files
        }
        try:
            return await compiled.verify_candidates(
                items,
                batch=batch,
                repo=self.prepared["repo"],
                brief=self.prepared["brief"],
                retrieval=self.prepared["retrieval"],
                frames=self.frames_runner,
                session=self.prepared["started"]["session"],
                diff_lines=diff_lines,
                diff_text=SLICE_SEPARATOR.join(chunk.text for chunk in self.chunks),
                plan_model=self.cfg["plan_model"],
                plan_thinking=self.cfg["plan_thinking"],
                ask_model=self.cfg["ask_model"],
                ask_thinking=self.cfg["ask_thinking"],
                cutoff=self.cfg["verify_cutoff"],
                clock=self.clock,
                cap=capped_level,
                to_level=to_level,
                enrich=parse_rules,
                generic=generic_reason,
                unavailable=compiled.runner_availability(self.prepared["started"]["session"])[1],
                limit=limit,
                start_index=start,
                files=self.scope.files,
            )
        except Exception as error:
            for item in items:
                item.pop("verification", None)
                item.pop("_steps", None)
            detail = _text(f"{type(error).__name__}: {error}", 160)
            self.not_checked.append(
                f"The host's checks failed for the {batch} candidates ({detail}); "
                f"the verifier frame judged {len(items)} candidate(s) instead."
            )
            return None

    async def _execute_deep_call(self) -> None:
        leads_future: Any = (
            asyncio.get_running_loop().create_future() if self.parallel else None
        )
        base_str = (
            str(self.spec.get("base")) if self.spec.get("repoDir") else self.spec.get("baseSha")
        ) or None
        deep_call = deep.run_deep(
            self.frames_runner,
            self.scope.files,
            self.scope.read_file,
            root=self.scope.root,
            rev=self.deep_rev,
            diff_text=self.diff_text_all,
            leads=[] if self.parallel else self.merged,
            context=self.shared,
            reference_view=self.reference_audience,
            rounds=self.cfg["deep_rounds"],
            model=self.cfg["deep_model"],
            thinking=self.cfg["deep_thinking"],
            cutoff=self.cfg["find_cutoff"],
            clock=self.clock,
            cap=capped_level,
            runner=self.runner,
            tests=self.test_options,
            to_level=to_level,
            title=_bounded(self.context.get("title"), TITLE_CHARS),
            description=_bounded(self.context.get("description"), DESCRIPTION_CHARS),
            base=base_str,
            guidance=self.guidance,
            enrich=parse_rules,
            generic=generic_reason,
            intent=self.stated,
            prepared=self.prepared,
            prove_leads=not self.hybrid,
            keep_session=self.hybrid,
            shapes=self.prepared["shapes"] if self.prepared else None,
            leads_future=leads_future,
            on_investigator=self.early_verify if self.parallel else None,
            shared_prefix=self.spec.get("systemPrefix") is True,
            risk_triage=(self.mode == "deep"),
        )

        if self.parallel:
            await self._gather_parallel_deep(deep_call, leads_future)
        elif self.hybrid:
            await self._gather_hybrid_deep(deep_call)
        else:
            self.deep_out = await deep_call

    async def _gather_parallel_deep(self, deep_call: Any, leads_future: Any) -> None:
        async def fast_then_leads() -> None:
            try:
                await self.fast_pass()
            finally:
                if not leads_future.done():
                    leads_future.set_result(list(self.merged))

        fast_result, deep_result = await asyncio.gather(
            fast_then_leads(), deep_call, return_exceptions=True
        )
        if isinstance(fast_result, BaseException):
            detail = _text(f"{type(fast_result).__name__}: {fast_result}", 160)
            self.incomplete.append(f"the fast pass failed ({detail})")
        if isinstance(deep_result, BaseException):
            raise deep_result
        self.deep_out = deep_result

    async def _gather_hybrid_deep(self, deep_call: Any) -> None:
        fast_candidates = [item for item in self.merged if not self.skip_candidate(item)]
        deep_result, fast_batch = await asyncio.gather(
            deep_call,
            self.verify_batch("fast", fast_candidates, 0, self.cfg["verify_candidates_cap"]),
            return_exceptions=True,
        )
        if isinstance(fast_batch, dict):
            self.verify_batches.append(fast_batch)
        if isinstance(deep_result, BaseException):
            raise deep_result
        self.deep_out = deep_result

    def _prune_refuted_deep_findings(self) -> None:
        assert self.deep_out is not None
        all_candidates = self.merged + self.deep_out["findings"]
        self.refuted = sum(1 for item in all_candidates if item.get("proof") == "refuted")
        self.merged = [item for item in self.merged if item.get("proof") != "refuted"]
        self.deep_out["findings"] = [
            item for item in self.deep_out["findings"] if item.get("proof") != "refuted"
        ]
        self.refuted_leads = self.deep_out["refuted"]
        refuted_ids = {id(entry["lead"]) for entry in self.refuted_leads}
        self.merged = [item for item in self.merged if id(item) not in refuted_ids]

    async def _integrate_deep_output(self) -> None:
        assert self.deep_out is not None
        self.investigators = self.deep_out["investigators"]
        self.dropped_generic += self.deep_out["generic"]
        self._prune_refuted_deep_findings()

        self.test_report = self.deep_out["tests"]
        if self.test_report["note"]:
            note = self.test_report["note"]
            self.not_checked.append(note[0].upper() + note[1:] + ".")
        self.assurance = deep.assurance(self.deep_out)

        before_merge = list(self.merged)
        self.merged, superseded = deep.merge(
            self.merged, self.deep_out["findings"], self.duplicate_records
        )
        self.duplicates += superseded
        for number, finding in enumerate(self.merged, 1):
            finding["id"] = number

        if self.hybrid:
            await self._run_hybrid_deep_batch(before_merge)

        _record_deep_diagnostics(
            self.deep_out,
            self.investigators,
            self.mode,
            self.run_fast,
            self.not_checked,
            self.incomplete,
        )

    async def _run_hybrid_deep_batch(self, before_merge: list[dict[str, Any]]) -> None:
        assert self.deep_out is not None
        assert self.prepared is not None
        _inherit_hybrid_verifications(self.merged, self.deep_out["findings"], before_merge)
        deep_candidates = [
            item
            for item in self.merged
            if not item.get("verification") and not self.skip_candidate(item)
        ]
        checked_so_far = sum(batch["checked"] for batch in self.verify_batches)
        remaining_cap = max(0, self.cfg["verify_candidates_cap"] - checked_so_far)
        try:
            deep_batch = await self.verify_batch("deep", deep_candidates, checked_so_far, remaining_cap)
            if deep_batch is not None:
                self.verify_batches.append(deep_batch)
        finally:
            session = self.prepared["started"]["session"]
            if session is not None:
                session.close()
        self.test_report = self.deep_out["tests"]
        self.test_report["runs"] = _format_session_runs(self.prepared["started"]["session"])

    async def _run_deep_stage(self) -> None:
        deep_started = self.clock()
        if self.chunks and self.run_deep_pass:
            try:
                await self._execute_deep_call()
            except Exception as error:
                detail = _text(f"{type(error).__name__}: {error}", 160)
                self.not_checked.append(
                    f"The deep pass failed ({detail}); this is the fast review only."
                )
                if self.prepared is not None and self.prepared["started"]["session"] is not None:
                    self.prepared["started"]["session"].close()
            if self.deep_out is not None:
                await self._integrate_deep_output()

        self.deep_ms = int((self.clock() - deep_started) * 1000)
        self.stages["deepMs"] = self.deep_ms

    def _ordered_sweep_chunks(self) -> list[Chunk]:
        if self.mode == "minimal":
            pool = [chunk for chunk in self.chunks if not _is_doc_only_chunk(chunk, self.members)]
            sweep_pool = pool or self.chunks
        else:
            sweep_pool = self.chunks
        return sorted(
            sweep_pool,
            key=lambda chunk: all(
                file_kind(member.path) == "test"
                for member in self.members.get(chunk.id, [chunk])
            ),
        )

    def _build_sweep_views(self) -> list[str]:
        already_lines = (
            "\n".join(
                f"- {finding['file']}:{finding['line']} "
                f"({finding.get('level') or finding['severity']}, {finding['category']}): {finding['claim']}"
                for finding in self.merged[:40]
            )
            or "(none)"
        )
        sweep_chunks = self._ordered_sweep_chunks()
        sweep_diff = SLICE_SEPARATOR.join(chunk.text for chunk in sweep_chunks)[:SWEEP_DIFF_CHARS]
        views = [
            f"Already-raised findings (do not repeat or reword these):\n{already_lines}",
            f"Diff under review:\n{sweep_diff}",
        ]
        callers = out_of_diff_callers(self.git, self.scope) if self.git is not None else ""
        if callers:
            views.append(f"Out-of-diff callers of changed/added definitions:\n{callers}")
        return views

    def _ingest_sweep_item(
        self,
        item: Any,
        sweep_reviewer: Reviewer,
        all_members: list[Chunk],
        swept_specific: list[dict[str, Any]],
    ) -> None:
        member = member_for(all_members, item)
        if member is None:
            return
        source = self.scope.read_file(member.path)
        normalized = normalize_findings(
            [item], sweep_reviewer, member, len(source) if source else None
        )
        for finding in normalized:
            _extras(item, finding)
            finding["source"] = "sweep"
            finding.setdefault("level", to_level(finding["severity"]))
            reason = generic_reason(finding)
            if reason:
                self.dropped_generic.append(reason)
            else:
                swept_specific.append(finding)

    def _collect_sweep_findings(self, raw_sweep: list[Any]) -> None:
        sweep_reviewer = Reviewer("sweep", "Gap sweep", "", ())
        all_members = [member for group in self.members.values() for member in group]
        swept_specific: list[dict[str, Any]] = []
        for item in raw_sweep[:6]:
            self._ingest_sweep_item(item, sweep_reviewer, all_members, swept_specific)

        if swept_specific:
            before_sweep = len(self.merged) + len(swept_specific)
            self.merged = dedupe_fast(self.merged + swept_specific, self.duplicate_records)
            self.duplicates += before_sweep - len(self.merged)

    async def _run_gap_sweep(self) -> None:
        can_sweep = (
            bool(self.chunks)
            and self.mode in ("fast", "deep", "both", "minimal")
            and not self.spec.get("only")
            and (self.cfg["budget"] - self.usage.tokens) >= 4_000
        )
        if not can_sweep:
            return

        sweep_started = self.clock()
        sweep_views = self._build_sweep_views()
        if self.merged and not self.hybrid:
            await self.early_verify("pre-sweep", self.merged)
        try:
            sweep_results = await self.frames_runner.run(
                "sweep",
                [("sweep", AUTOREVIEW_SWEEP_TASK, sweep_views)],
                contract=AUTOREVIEW_FINDINGS_CONTRACT,
                model=self.cfg["model"],
                thinking=self.cfg["thinking"],
                context=self.frames_context,
                cutoff=self.cfg["find_cutoff"],
            )
            raw_sweep = sweep_results[0] if sweep_results else None
            if isinstance(raw_sweep, list):
                self._collect_sweep_findings(raw_sweep)
        except Exception as error:
            detail = _text(f"{type(error).__name__}: {error}", 160)
            self.not_checked.append(f"The gap-sweep pass failed ({detail}).")
        self.stages["sweepMs"] = int((self.clock() - sweep_started) * 1000)

    def _prepare_verify_queue(
        self, candidates: list[dict[str, Any]], verify_budget: int
    ) -> tuple[
        list[dict[str, Any]],
        list[list[str]],
        list[str],
        list[int | None],
        list[dict[str, Any]],
        list[tuple[dict[str, Any], Any]],
    ]:
        items: list[list[str]] = []
        sources: list[str] = []
        counts: list[int | None] = []
        to_verify: list[dict[str, Any]] = []
        unverified: list[dict[str, Any]] = []
        early_verdicts: list[tuple[dict[str, Any], Any]] = []

        for finding in sorted(candidates, key=level_rank):
            if finding.get("host_confirmed"):
                continue
            early = finding.pop("_early", None)
            if early is not None:
                early_verdicts.append((finding, early))
                continue
            built = self.verify_views(finding)
            if built is None:
                continue
            views, source_text, count = built
            cost = _estimate_tokens(AUTOREVIEW_VERIFIER_TASK, *views, output=600)
            if self.verify_estimate + cost > verify_budget:
                unverified.append(dict(finding, verification="not verified (budget)"))
                continue
            self.verify_estimate += cost
            items.append(views)
            sources.append(source_text)
            counts.append(count)
            to_verify.append(finding)

        return to_verify, items, sources, counts, unverified, early_verdicts

    async def _run_split_verifiers(
        self,
        to_verify: list[dict[str, Any]],
        items: list[list[str]],
        loki_indices: list[int],
        other_indices: list[int],
        loki_model: str | None,
    ) -> list[Any]:
        loki_task = self.run_verifier(
            [to_verify[idx] for idx in loki_indices],
            [items[idx] for idx in loki_indices],
            model_override=loki_model,
        )
        if other_indices:
            other_task = self.run_verifier(
                [to_verify[idx] for idx in other_indices],
                [items[idx] for idx in other_indices],
            )
            loki_verdicts, other_verdicts = await asyncio.gather(loki_task, other_task)
        else:
            loki_verdicts, other_verdicts = await loki_task, []

        verdicts: list[Any] = [None] * len(to_verify)
        for idx, verdict in zip(loki_indices, loki_verdicts):
            verdicts[idx] = verdict
        for idx, verdict in zip(other_indices, other_verdicts):
            verdicts[idx] = verdict
        return verdicts

    async def _execute_verifiers(
        self, to_verify: list[dict[str, Any]], items: list[list[str]]
    ) -> list[Any]:
        if not to_verify:
            return []
        loki_indices = [
            idx for idx, finding in enumerate(to_verify) if finding.get("source") == "static:loki"
        ]
        other_indices = [
            idx for idx, finding in enumerate(to_verify) if finding.get("source") != "static:loki"
        ]
        loki_model = self.cfg["loki_verify_model"]
        if not loki_indices or loki_model == self.cfg["verify_model"]:
            return await self.run_verifier(to_verify, items)
        return await self._run_split_verifiers(
            to_verify, items, loki_indices, other_indices, loki_model
        )

    def _merge_verifier_inputs(
        self,
        to_verify: list[dict[str, Any]],
        verdicts: list[Any],
        sources: list[str],
        counts: list[int | None],
        early_verdicts: list[tuple[dict[str, Any], Any]],
    ) -> dict[Any, Any]:
        for idx, finding in enumerate(to_verify):
            needed_list = finding.get("_needed_code")
            if isinstance(needed_list, list) and needed_list:
                extra = "\n\n".join(needed_list)
                sources[idx] = f"{sources[idx]}\n{extra}"
        for finding, early in early_verdicts:
            to_verify.append(finding)
            verdicts.append(early["verdict"])
            sources.append(early["source"])
            counts.append(early["count"])
        return {finding["id"]: verdict for finding, verdict in zip(to_verify, verdicts)}

    def _annotate_verified_findings(
        self, by_id: dict[Any, Any], before_lines: dict[Any, int]
    ) -> None:
        for finding in self.confirmed + self.uncertain:
            if isinstance(finding.get("check"), dict):
                candidate_label = finding["check"].get("candidate") or "beyond-cap"
                finding["verified_by"] = f"verifier:{candidate_label}"
            if finding["line"] != before_lines.get(finding["id"]):
                finding.pop("end_line", None)
                finding.pop("replacement", None)

        for finding in self.confirmed:
            verdict = by_id.get(finding["id"])
            set_level(finding, final_level(finding, verdict))
            if isinstance(verdict, dict) and verdict.get("introduced") == "before":
                finding["preexisting"] = True

    def _record_verifier_failures(self, verdicts: list[Any]) -> None:
        failed = sum(1 for verdict in verdicts if isinstance(verdict, (Incomplete, FrameError)))
        if not failed:
            return
        late = sum(
            1
            for verdict in verdicts
            if isinstance(verdict, FrameError) and verdict.error == DEADLINE_ERROR
        )
        reason = (
            "the review deadline was reached"
            if late == failed
            else "the verifier frame ran out or failed"
        )
        self.incomplete.append(f"{failed} finding(s) could not be verified ({reason})")

    def _apply_verifier_outcomes(
        self,
        to_verify: list[dict[str, Any]],
        verdicts: list[Any],
        sources: list[str],
        counts: list[int | None],
        early_verdicts: list[tuple[dict[str, Any], Any]],
    ) -> None:
        by_id = self._merge_verifier_inputs(to_verify, verdicts, sources, counts, early_verdicts)
        for finding in to_verify:
            verdict = by_id.get(finding["id"])
            if isinstance(verdict, dict) and verdict.get("verdict") == "unclear":
                finding["unclear"] = True

        before_lines = {finding["id"]: finding["line"] for finding in to_verify}
        self.confirmed, self.uncertain, self.rejected = apply_verdicts(
            to_verify, list(verdicts), sources, counts
        )
        self._annotate_verified_findings(by_id, before_lines)
        self._record_verifier_failures(verdicts)

    def _active_blind_repo(self) -> Any:
        if self.deep_out and self.deep_out.get("repo"):
            return self.deep_out["repo"]
        if self.compiled_out and self.compiled_out.get("repo"):
            return self.compiled_out["repo"]
        if self.prepared:
            return self.prepared.get("repo")
        return None

    def _append_blind_context_sections(self, finding: dict[str, Any], window: str) -> str:
        blind_repo = self._active_blind_repo()
        if blind_repo is not None and finding.get("citations"):
            cited = deep.cited_windows(blind_repo, finding)
            if cited:
                window = f"{window}\n\n{cited}"

        related = finding.get("_related_code")
        if related:
            window = f"{window}\n\nOTHER PLACES DEFINING OR USING THESE NAMES:\n{related}"

        needed_list = finding.get("_needed_code")
        if isinstance(needed_list, list) and needed_list:
            needed = "\n\n".join(needed_list)
            window = f"{window}\n\nADDITIONAL CODE CHECKED BY VERIFIER:\n{needed[:12_000]}"
        return window

    def _blind_material(self, finding: dict[str, Any]) -> tuple[str, str] | None:
        source = self.scope.read_file(finding["file"])
        if not source:
            return None
        hunk = hunk_for(self.by_path.get(finding["file"]), finding["line"])
        window = source_window(source, finding["line"], line_limit=RECHECK_LINE_CHARS)
        enriched = self._append_blind_context_sections(finding, window)
        hunk_text = "\n".join(render_hunk(hunk)) if hunk else "(no hunk)"
        return hunk_text, enriched

    async def _run_blind_stage(self) -> None:
        if not self.confirmed:
            return
        try:
            self.blind_out = await blind.check(
                self.rlm, sorted(self.confirmed, key=level_rank), self._blind_material
            )
        except Exception as error:
            self.blind_out = {
                "asked": 0,
                "unavailable": _text(f"{type(error).__name__}: {error}", 160),
            }

        refuted_note = (
            "the blind check refuted it: the judge seat, and a model of another "
            "family it escalated the dispute to, did not find the claim in the code"
        )
        self.uncertain += [
            dict(finding, verification=refuted_note)
            for finding in self.confirmed
            if finding.get("blind") == "refuted"
        ]
        self.confirmed = [
            finding for finding in self.confirmed if finding.get("blind") != "refuted"
        ]
        self.usage.cost += float(self.blind_out.get("costUsd") or 0)

    def _collect_verification_candidates(
        self,
    ) -> tuple[list[dict[str, Any]], list[dict[str, Any]], int, int]:
        self.open_earlier = [
            item for item in self.earlier if item["status"] in ("still_present", "unknown")
        ]
        raw_static = self.static_out.get("findings") if isinstance(self.static_out, dict) else None
        static_findings = list(raw_static) if isinstance(raw_static, list) else []
        self.found_static = static_findings + static_checks.orphaned_private_helpers(
            self.git, self.scope
        )
        if self.minimal_started and self.minimal_started.get("observed"):
            self.found_static += list(self.minimal_started["observed"])

        (
            candidates,
            self.also_raised,
            self.contradicted,
            hybrid_confirmed,
            filter_dupes,
            hybrid_refuted,
            hybrid_undecided,
        ) = _filter_verify_candidates(
            self.merged,
            self.found_static,
            self.static_out,
            self.others,
            self.open_earlier,
            self.hybrid,
            self.duplicate_records,
        )
        self.duplicates += filter_dupes
        return candidates, hybrid_confirmed, hybrid_refuted, hybrid_undecided

    def _finalize_verification_results(
        self,
        observed: list[dict[str, Any]],
        unverified: list[dict[str, Any]],
        hybrid_confirmed: list[dict[str, Any]],
        hybrid_refuted: int,
        hybrid_undecided: int,
    ) -> None:
        raw_lessons = self.spec.get("lessons")
        lesson_items = (
            [item for item in raw_lessons if isinstance(item, dict)]
            if isinstance(raw_lessons, list)
            else []
        )
        apply_lessons(self.confirmed, lesson_items)
        self.confirmed += observed
        self.uncertain += unverified
        if self.hybrid:
            self.confirmed += hybrid_confirmed
            self.rejected += [{}] * hybrid_refuted
            (
                self.verification,
                self.program_records,
                self.program_stats,
                self.assurance,
            ) = _summarize_hybrid_verification(
                self.verify_batches, hybrid_undecided, self.prepared, self.assurance, self.not_checked
            )
        if unverified:
            self.incomplete.append(
                f"{len(unverified)} finding(s) were not verified within the token budget"
            )
        self._record_throttle_note()

    async def _run_verifier_with_fallback(
        self,
        to_verify: list[dict[str, Any]],
        items: list[list[str]],
        sources: list[str],
        counts: list[int | None],
        early_verdicts: list[tuple[dict[str, Any], Any]],
    ) -> None:
        try:
            verdicts = await self._execute_verifiers(to_verify, items)
            self._apply_verifier_outcomes(to_verify, verdicts, sources, counts, early_verdicts)
        except Exception as error:
            self.confirmed, self.rejected = [], []
            self.uncertain = [
                dict(finding, verification="not verified (the verifier stage failed)")
                for finding in to_verify
            ]
            detail = _text(f"{type(error).__name__}: {error}", 160)
            self.incomplete.append(f"the verifier stage failed ({detail})")

    async def _run_verification_and_blind_stages(self) -> None:
        verify_started = self.clock()
        (
            candidates,
            hybrid_confirmed,
            hybrid_refuted,
            hybrid_undecided,
        ) = self._collect_verification_candidates()

        verify_budget = max(0, self.cfg["budget"] - self.usage.tokens)
        observed = [finding for finding in candidates if finding.get("host_confirmed")]
        await asyncio.gather(*self.early_tasks, return_exceptions=True)
        to_verify, items, sources, counts, unverified, early_verdicts = self._prepare_verify_queue(
            candidates, verify_budget
        )

        if to_verify or early_verdicts:
            await self._run_verifier_with_fallback(
                to_verify, items, sources, counts, early_verdicts
            )

        await self._run_blind_stage()
        self._finalize_verification_results(
            observed, unverified, hybrid_confirmed, hybrid_refuted, hybrid_undecided
        )
        self.verify_ms = int((self.clock() - verify_started) * 1000) + self.recheck_ms
        self.stages["verifyMs"] = int((self.clock() - verify_started) * 1000)
        if self.program_ms:
            self.stages["programMs"] = self.program_ms

    def _record_throttle_note(self) -> None:
        if not self.frames_runner.throttled:
            return
        lost = sum(
            1
            for item in self.frames_runner.timings
            if item.get("status") in ("failed", "timeout")
            and self.is_throttled_fn(FrameError({"error": item.get("error", "")}))
        )
        throttled_list = ", ".join(
            f"{name or 'the model'} (frames in flight lowered to {limit})"
            for name, limit in self.frames_runner.throttled.items()
        )
        suffix = f"; {lost} frame(s) were lost to it." if lost else "; no frame was lost to it."
        self.not_checked.append(f"The provider throttled {throttled_list}{suffix}")

    def _check_all_frames_failed(self) -> None:
        statuses = [
            item["status"]
            for item in self.frames_runner.timings
            if item["phase"] in ("find", "deep")
        ]
        if statuses and all(status == "failed" for status in statuses):
            first = next(
                (item for item in self.frames_runner.timings if item.get("error")), None
            )
            detail = f": {first['error']}" if first else ""
            raise ReviewError(f"the model provider failed every frame of this review{detail}")

    def _build_usage_payload(self) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "inputTokens": self.usage.input,
            "outputTokens": self.usage.output,
            "costUsd": round(self.usage.cost, 6),
            "frames": self.usage.frames,
            "tokens": self.usage.tokens,
            "budget": self.cfg["cap"],
            "byPhase": self.usage.by_phase,
        }
        if self.blind_out and "calls" in self.blind_out:
            payload["judge"] = {
                key: self.blind_out[key] for key in ("calls", "costUsd", "unpricedCalls")
            }
        return payload

    def _partition_findings_for_output(
        self,
    ) -> tuple[list[dict[str, Any]], list[dict[str, Any]], int]:
        preexisting = sorted(
            (item for item in self.confirmed if item.get("preexisting")), key=level_rank
        )
        self.confirmed = [item for item in self.confirmed if not item.get("preexisting")]
        self.confirmed.sort(key=level_rank)
        self.uncertain.sort(key=level_rank)
        findings = [_public(item, "confirmed") for item in self.confirmed] + [
            _public(item, "uncertain") for item in self.uncertain
        ]
        medium_index = LEVELS.index("medium")
        named_preexisting = [
            _public(item, "confirmed")
            for item in preexisting
            if LEVELS.index(item.get("level") or to_level(item["severity"])) <= medium_index
        ][:MAX_PREEXISTING]
        return findings, named_preexisting, len(preexisting) - len(named_preexisting)

    def _build_dropped_payload(self, unnamed_preexisting: int) -> dict[str, Any]:
        refuted_by_investigator = [
            {
                "file": entry["lead"]["file"],
                "line": entry["lead"]["line"],
                "claim": entry["lead"]["claim"],
                "by": entry["lens"],
                "why": entry["why"],
                "evidence": [f"{cite['path']}:{cite['line']}" for cite in entry["citations"]],
            }
            for entry in self.refuted_leads
        ]
        dropped: dict[str, Any] = {
            "rejected": len(self.rejected),
            "duplicates": self.duplicates,
            "generic": len(self.dropped_generic),
            "refutedByTest": self.refuted,
            "contradicted": len(self.contradicted),
            "duplicateOf": self.duplicate_records,
            "refutedByInvestigator": refuted_by_investigator,
        }
        if unnamed_preexisting:
            dropped["preexisting"] = unnamed_preexisting
        return dropped

    def _build_timing_payload(self) -> dict[str, Any]:
        reference = (
            dict(self.prepared["brief"].reference_stats)
            if self.prepared and self.prepared["brief"].reference_stats
            else None
        )
        return {
            "totalMs": int((self.clock() - self.started) * 1000),
            "scopeMs": self.scope_ms,
            "findMs": self.find_ms,
            "verifyMs": self.verify_ms,
            "deepMs": self.deep_ms,
            "programMs": self.program_ms,
            "stages": self.stages,
            "frames": self.frames_runner.timings,
            "investigators": self.investigators,
            "program": self.program_records,
            "reference": reference,
        }

    def _build_model_metadata_payload(self) -> dict[str, Any]:
        has_deep = self.deep_out is not None
        has_compiled = self.compiled_out is not None
        plan_style = (
            (self.program_stats or {}).get("planner", {}).get("style") if has_compiled else None
        )
        return {
            "model": self.cfg["model"],
            "verifyModel": self.cfg["verify_model"],
            "thinking": self.cfg["thinking"],
            "verifyThinking": self.cfg["verify_thinking"],
            "mode": _resolve_final_mode(self.mode, self.compiled_out, self.hybrid, self.deep_out),
            "deepModel": self.cfg["deep_model"] if has_deep else None,
            "finderModels": self.cfg["finder_models"] or None,
            "deepThinking": self.cfg["deep_thinking"] if has_deep else None,
            "planModel": self.cfg["plan_model"] if has_compiled else None,
            "planThinking": self.cfg["plan_thinking"] if has_compiled else None,
            "askModel": self.cfg["ask_model"] if has_compiled else None,
            "askThinking": self.cfg["ask_thinking"] if has_compiled else None,
            "planStyle": plan_style,
        }

    def _build_result_payload(self) -> dict[str, Any]:
        self.confirmed, merged_causes = group_root_causes(self.confirmed, self.duplicate_records)
        self.duplicates += merged_causes
        self._check_all_frames_failed()

        findings, named_preexisting, unnamed_preexisting = self._partition_findings_for_output()
        self.not_checked += self.incomplete
        static_payload = (
            {
                "stages": self.static_out["stages"],
                "findings": len(self.found_static),
                "contradicted": self.contradicted,
            }
            if self.static_out
            else None
        )
        brief_payload = (
            {
                "text": self.prepared["brief"].text[:BRIEF_RECORD_CHARS],
                "lenses": list(self.prepared["brief"].lenses),
            }
            if self.prepared
            else None
        )

        result: dict[str, Any] = {
            "complete": not self.incomplete,
            "label": self.scope.label,
            "files": len(self.scope.files),
            "added": sum(item.added for item in self.scope.files),
            "removed": sum(item.removed for item in self.scope.files),
            "findings": findings,
            "alsoRaised": self.also_raised,
            "earlier": self.earlier,
            "dropped": self._build_dropped_payload(unnamed_preexisting),
            "timing": self._build_timing_payload(),
            "usage": self._build_usage_payload(),
            **self._build_model_metadata_payload(),
            "verification": self.verification,
            "static": static_payload,
            "blind": self.blind_out,
            "program": self.program_stats,
            "assurance": self.assurance,
            "tests": self.test_report,
            "brief": brief_payload,
            "guides": sum(1 for name in self.guide_names if os.sep in name),
            "guideNames": self.guide_names,
            "notChecked": self.not_checked,
            "incomplete": self.incomplete,
            "diffLines": diff_line_ranges(self.post_files),
        }
        if named_preexisting:
            result["preexisting"] = named_preexisting
        return result

    def _collect_compiled_stage_output(self) -> None:
        if self.compiled_out is None:
            return
        (
            self.merged,
            compiled_dupes,
            compiled_generic,
            self.refuted,
            self.test_report,
            self.assurance,
            self.program_stats,
            self.program_records,
        ) = _collect_compiled_findings(self.compiled_out, self.not_checked)
        self.duplicates += compiled_dupes
        self.dropped_generic += compiled_generic

    def _should_overlap_fast_and_deep(self) -> bool:
        overlap_requested = self.spec.get("overlap") is True or self.changed_lines >= OVERLAP_LINES
        return (
            bool(self.chunks)
            and self.run_fast
            and self.run_deep_pass
            and not self.hybrid
            and overlap_requested
        )

    async def execute(self) -> dict[str, Any]:
        self._prepare_context_and_chunks()

        recheck_started = self.clock()
        recheck_task = asyncio.create_task(
            _run_recheck_stage(
                self.rlm,
                self.spec,
                self.scope,
                self.git,
                self.frames_runner,
                self.cfg["budget"],
                self.cfg["verify_model"],
                self.cfg["verify_thinking"],
                self.cfg["find_cutoff"],
                self.incomplete,
            )
        )

        self.find_started = self.clock()
        self.find_budget = int(max(0, self.cfg["budget"] - self.usage.tokens) * FIND_SHARE)

        await self._run_compiled_pass()
        self.stages["scopeMs"] = self.scope_ms
        self.stages["recheckMs"] = self.recheck_ms
        await self._run_static_stage()
        self._prepare_deep_or_minimal_tests()
        await self._build_frame_contexts()

        self.parallel = self._should_overlap_fast_and_deep()
        if not self.parallel:
            await self.fast_pass()

        self._collect_compiled_stage_output()
        if recheck_task.done():
            self.earlier = await recheck_task
        self.open_earlier = [
            item for item in self.earlier if item["status"] in ("still_present", "unknown")
        ]

        await self._run_deep_stage()
        await self._run_gap_sweep()

        self.earlier = await recheck_task
        self.recheck_ms = int((self.clock() - recheck_started) * 1000)
        self.stages["recheckMs"] = self.recheck_ms

        await self._run_verification_and_blind_stages()
        return self._build_result_payload()
