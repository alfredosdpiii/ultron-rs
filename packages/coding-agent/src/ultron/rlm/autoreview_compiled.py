"""Compiled mode of `ultron autoreview`: one planner writes a program; host executes it.

1. Program: steps with ids, `needs`, read-only lookups, sandboxed tests, `for_each`,
   `ask`, `assert`, `finding`, and `when` conditions.
2. Validation: checked against language and coverage rules before execution; one repair
   round on failure, else fallback to `both` mode.
3. Execution: deterministic host control flow; small model called only at `ask` points
   and unresolved/contradicted findings (`RESOLVE_TASK`).
4. Findings: deterministic checks skip the verifier; `ask`-rooted findings go through
   the verifier. Asserts that held become the review's assurance.
"""
from __future__ import annotations

import dataclasses
import json
from typing import Any, Callable

import autoreview_deep as deep
import autoreview_tests as testing
from autoreview_compiled_cells import (
    CELL_CONTRACT as CELL_CONTRACT,
    CELL_RUNNER as CELL_RUNNER,
    CellRunner as CellRunner,
    CellSession as CellSession,
    RvError as RvError,
    _public_value as _public_value,
    run_cells as run_cells,
)
from autoreview_compiled_exec import Interpreter as Interpreter
from autoreview_compiled_plan import (
    ANSWERS as ANSWERS,
    ASK_CONTEXT_CHARS as ASK_CONTEXT_CHARS,
    ASK_CONTRACT as ASK_CONTRACT,
    ASK_MATERIAL_CHARS as ASK_MATERIAL_CHARS,
    CANDIDATE_READ_RADIUS as CANDIDATE_READ_RADIUS,
    CAPPED as CAPPED,
    CELL_OUTPUT_CHARS as CELL_OUTPUT_CHARS,
    CELL_TIMEOUT_S as CELL_TIMEOUT_S,
    CELL_TRANSCRIPT_CHARS as CELL_TRANSCRIPT_CHARS,
    CELL_TRANSCRIPT_FULL as CELL_TRANSCRIPT_FULL,
    CELL_VALUE_CHARS as CELL_VALUE_CHARS,
    CELL_VALUE_ITEMS as CELL_VALUE_ITEMS,
    CITATION_READ_RADIUS as CITATION_READ_RADIUS,
    CLOSEST_TEST_RADIUS as CLOSEST_TEST_RADIUS,
    DEFAULT_CELL_THINKING as DEFAULT_CELL_THINKING,
    DEFAULT_FOR_EACH_ITEMS as DEFAULT_FOR_EACH_ITEMS,
    DEFAULT_PLAN_CELLS as DEFAULT_PLAN_CELLS,
    DEFAULT_PLAN_STYLE as DEFAULT_PLAN_STYLE,
    DEFAULT_VERIFY_CANDIDATES as DEFAULT_VERIFY_CANDIDATES,
    DEPLOY_GLOBS as DEPLOY_GLOBS,
    EVIDENCE_CHARS as EVIDENCE_CHARS,
    ITERABLE as ITERABLE,
    LARGE_DIFF_LINES as LARGE_DIFF_LINES,
    LARGE_PROGRAM_STEPS as LARGE_PROGRAM_STEPS,
    LARGE_STEPS as LARGE_STEPS,
    LEVEL_TO_OLD as LEVEL_TO_OLD,
    LEVELS as LEVELS,
    LOOKUPS as LOOKUPS,
    MAX_ASK_MATERIAL as MAX_ASK_MATERIAL,
    MAX_ASKS as MAX_ASKS,
    MAX_EXTRA_STEPS as MAX_EXTRA_STEPS,
    MAX_FINDINGS as MAX_FINDINGS,
    MAX_FOR_EACH_ITEMS as MAX_FOR_EACH_ITEMS,
    MAX_HOLDS as MAX_HOLDS,
    MAX_PLAN_CELLS as MAX_PLAN_CELLS,
    MAX_PROGRAM_STEPS as MAX_PROGRAM_STEPS,
    MAX_QUESTION_CHARS as MAX_QUESTION_CHARS,
    MAX_STEPS as MAX_STEPS,
    MAX_TEMPLATE_STEPS as MAX_TEMPLATE_STEPS,
    MAX_VERIFY_CANDIDATES as MAX_VERIFY_CANDIDATES,
    OLD_LEVELS as OLD_LEVELS,
    OPS as OPS,
    PLAN_STYLES as PLAN_STYLES,
    PROGRAM_CONTRACT as PROGRAM_CONTRACT,
    RECORD_CHARS as RECORD_CHARS,
    RESULT_CHARS as RESULT_CHARS,
    RETRIEVAL_CHARS as RETRIEVAL_CHARS,
    RETRIEVAL_HITS as RETRIEVAL_HITS,
    STATUSES as STATUSES,
    TEST_OPS as TEST_OPS,
    Program as Program,
    _assert_leaves as _assert_leaves,
    _check_step as _check_step,
    _CONTAINS as _CONTAINS,
    _COUNT as _COUNT,
    _count_of as _count_of,
    _count_predicate as _count_predicate,
    _EFFECT as _EFFECT,
    _ERROR_PATH as _ERROR_PATH,
    _flat as _flat,
    _GUARD as _GUARD,
    _HIT as _HIT,
    _ID as _ID,
    _ids as _ids,
    _items_of as _items_of,
    _MANIFEST as _MANIFEST,
    _MIGRATION as _MIGRATION,
    _normalize_step as _normalize_step,
    _parse_predicate as _parse_predicate,
    _PLACEHOLDER as _PLACEHOLDER,
    _public_step as _public_step,
    _references as _references,
    _render as _render,
    _RETRY as _RETRY,
    _step_texts as _step_texts,
    _TITLE_COUNT as _TITLE_COUNT,
    _when as _when,
    _WORD as _WORD,
    _WORKFLOW as _WORKFLOW,
    as_dict,
    as_list,
    coverage_items as coverage_items,
    grounded as grounded,
    limits_for as limits_for,
    shape_items as shape_items,
    uncovered_items as uncovered_items,
    validate as validate,
)
from autoreview_compiled_verify import (
    _COMMENTISH as _COMMENTISH,
    _ERRORISH as _ERRORISH,
    _finding_fields as _finding_fields,
    _INPUTISH as _INPUTISH,
    _quote_in as _quote_in,
    _quote_in_reads as _quote_in_reads,
    _WORDS as _WORDS,
    ask_material as ask_material,
    candidate_shape as candidate_shape,
    planner_views as planner_views,
    retrieve as retrieve,
    runner_availability as runner_availability,
    template_steps as template_steps,
    verify_candidates as verify_candidates,
)
from infer_api import FrameError, Incomplete
from review_api import FileDiff, _text
from review_prompts import COMPILED_REPAIR, cell_planner_task, compiled_planner_task


def _dump_program(program_json: dict[str, Any], dump_path: str | None) -> None:
    if not dump_path:
        return
    with open(dump_path, "w", encoding="utf-8") as handle:
        json.dump(program_json, handle, indent=1)


def _format_uncovered_lines(
    declared: list[str],
    coverage: list[dict[str, str]],
) -> list[str]:
    by_id = {item["id"]: item for item in coverage}
    out: list[str] = []
    for item in declared:
        cid, _, why = item.partition(":")
        clean_id = cid.strip()
        text = by_id.get(clean_id, {}).get("text", clean_id)
        suffix = f": {why.strip()}" if why.strip() else ""
        out.append(f"{clean_id} ({text}){suffix}")
    return out


def _trim_test_runs(session: Any) -> list[dict[str, Any]]:
    runs = list(session.records) if session is not None else []
    return [
        {key: value for key, value in record.items() if key != "output"}
        | {"output": record["output"][-600:]}
        for record in runs
    ]


def _populate_compiled_output(
    out: dict[str, Any],
    interpreter: Interpreter,
    *,
    coverage: list[dict[str, str]],
    declared: list[str],
    planner: dict[str, Any],
    summary: str,
    session: Any,
    materialised: int | None = None,
) -> None:
    out["findings"] += interpreter.findings
    out["dropped"] = interpreter.dropped
    out["generic"] = interpreter.generic_dropped
    out["records"] = interpreter.records
    out["refuted"] = interpreter.refuted
    out["uncovered"] = _format_uncovered_lines(declared, coverage)

    stats = interpreter.stats()
    if materialised is not None:
        stats["findings"]["materialised"] = materialised
    out["stats"] = {
        **stats,
        "planner": planner,
        "summary": summary,
        "retrieval": out["retrieval"],
        "coverage": {
            "items": len(coverage),
            "covered": len(coverage) - len(declared),
            "uncovered": [item.split(":", 1)[0].strip() for item in declared],
        },
    }
    out["assurance"] = interpreter.assurance()
    out["tests"]["runs"] = _trim_test_runs(session)


def _resolve_cell_sandbox(
    cell_sandbox: Any,
    started: dict[str, Any],
    tests: dict[str, Any] | None,
) -> Any:
    if cell_sandbox != "detect":
        return cell_sandbox
    if started.get("sandbox") is not None:
        return started["sandbox"]
    return testing.detect_sandbox(image=as_dict(tests).get("image"))


def _plan_token_total(frames: Any) -> int:
    return sum(
        int(item.get("tokens") or 0) for item in frames.timings if item.get("phase") == "plan"
    )


def _record_cell_planner_stats(
    planner: dict[str, Any],
    cell_run: dict[str, Any],
    cell_session: CellSession,
    frames: Any,
    began: float,
    clock: Callable[[], float],
) -> None:
    planner["ms"] = int((clock() - began) * 1000)
    planner["tokens"] = _plan_token_total(frames)
    planner["cells"] = len(cell_session.cells)
    planner["cellTokens"] = list(as_list(cell_run.get("cell_tokens")))
    planner["status"] = cell_run["status"]


def _collect_cell_declared(
    coverage: list[dict[str, str]],
    steps: list[dict[str, Any]],
    cell_session: CellSession,
) -> list[str]:
    declared = list(cell_session.declared)
    covered_markers = declared + [f"{sid}:" for sid in cell_session.covered]
    for item in uncovered_items(coverage, steps, covered_markers):
        declared.append(f"{item['id']}: not covered by the planner")
    return declared


@dataclasses.dataclass
class _RunContext:
    repo: deep.Repo
    brief: Any
    diff_text: str
    diff_lines: dict[str, set[int]]
    context: str
    intent: str
    guidance: str
    retrieved: dict[str, Any]
    coverage: list[dict[str, str]]
    limits: tuple[int, int]
    frames: Any
    plan_model: str | None
    plan_thinking: str | None
    ask_model: str | None
    ask_thinking: str | None
    cutoff: float | None
    clock: Callable[[], float]
    cap: Callable[..., str]
    tests: dict[str, Any] | None
    to_level: Callable[[Any], str | None] | None
    enrich: Callable[[Any, dict[str, Any]], None] | None
    generic: Callable[[dict[str, Any]], str | None] | None
    program: Any
    dump_path: str | None
    plan_style: str
    plan_cells: int
    cell_sandbox: Any
    python: str
    planner: dict[str, Any]


def _make_interpreter(
    program: Program,
    ctx: _RunContext,
    session: Any,
    unavailable: set[tuple[str, str]],
) -> Interpreter:
    return Interpreter(
        program,
        ctx.repo,
        frames=ctx.frames,
        session=session,
        diff_lines=ctx.diff_lines,
        ask_model=ctx.ask_model,
        ask_thinking=ctx.ask_thinking,
        cutoff=ctx.cutoff,
        clock=ctx.clock,
        cap=ctx.cap,
        to_level=ctx.to_level,
        enrich=ctx.enrich,
        generic=ctx.generic,
        unavailable=unavailable,
        max_steps=ctx.limits[1],
    )


async def _run_cell_plan_mode(
    out: dict[str, Any],
    ctx: _RunContext,
    *,
    sandbox: Any,
    session: Any,
    views: list[str],
    tests_allowed: bool,
    unavailable: set[tuple[str, str]],
) -> None:
    interpreter = _make_interpreter(Program([], ""), ctx, session, unavailable)
    began = ctx.clock()
    cell_run = await run_cells(
        ctx.frames,
        views=views,
        task=cell_planner_task(tests_allowed),
        plan_model=ctx.plan_model,
        plan_thinking=ctx.plan_thinking,
        cutoff=ctx.cutoff,
        clock=ctx.clock,
        interpreter=interpreter,
        coverage=ctx.coverage,
        limits=ctx.limits,
        sandbox=sandbox,
        cells=max(1, min(MAX_PLAN_CELLS, int(ctx.plan_cells))),
        python=ctx.python,
    )
    cell_session: CellSession = cell_run["session"]
    _record_cell_planner_stats(ctx.planner, cell_run, cell_session, ctx.frames, began, ctx.clock)
    out["notes"] = list(as_list(cell_run.get("notes")))

    if cell_run["status"] == "failed" and not cell_session.cells:
        reason = cell_run.get("error") or "; ".join(cell_run["notes"])
        out["fallback"] = reason or "the planner cells failed"
        return

    steps = [step for sid, step in interpreter.steps_by_id.items() if "[" not in sid]
    declared = _collect_cell_declared(ctx.coverage, steps, cell_session)
    validated = Program(steps, "", declared)
    out["program"] = {**validated.as_json(), "style": "cell", "cells": cell_session.cells}
    _dump_program(out["program"], ctx.dump_path)
    _populate_compiled_output(
        out,
        interpreter,
        coverage=ctx.coverage,
        declared=declared,
        planner=ctx.planner,
        summary="",
        session=session,
        materialised=cell_session.materialised,
    )


def _reply_failure_text(reply: Any) -> str:
    error = getattr(reply, "error", None)
    if error:
        return _text(error, 160)
    status = getattr(reply, "status", None)
    return _text(status if status else "no reply", 160)


async def _run_frame_attempt(
    attempt: int,
    errors: list[str],
    *,
    frames: Any,
    task: str,
    views: list[str],
    plan_model: str | None,
    plan_thinking: str | None,
    cutoff: float | None,
    coverage: list[dict[str, str]],
    max_planned: int,
) -> tuple[Program | None, list[str], str | None]:
    extra: list[str] = []
    if attempt > 0:
        bullet_errors = "\n".join(f"- {item}" for item in errors[:20])
        extra.append(COMPILED_REPAIR.format(errors=bullet_errors))
    replies = await frames.run(
        "plan",
        [("planner", task, views + extra)],
        contract=PROGRAM_CONTRACT,
        model=plan_model,
        thinking=plan_thinking,
        cutoff=cutoff,
    )
    reply = replies[0]
    if isinstance(reply, (Incomplete, FrameError)) or not isinstance(reply, dict):
        return None, errors, f"the planner frame failed ({_reply_failure_text(reply)})"
    validated, next_errors = validate(reply, coverage, max_planned)
    return validated, next_errors, None


async def _plan_frame_program(
    out: dict[str, Any],
    *,
    frames: Any,
    views: list[str],
    tests_allowed: bool,
    plan_model: str | None,
    plan_thinking: str | None,
    cutoff: float | None,
    clock: Callable[[], float],
    coverage: list[dict[str, str]],
    max_planned: int,
    planner: dict[str, Any],
) -> Program | None:
    task = compiled_planner_task(tests_allowed)
    began = clock()
    validated: Program | None = None
    errors: list[str] = []

    for attempt in range(2):
        validated, errors, frame_error = await _run_frame_attempt(
            attempt,
            errors,
            frames=frames,
            task=task,
            views=views,
            plan_model=plan_model,
            plan_thinking=plan_thinking,
            cutoff=cutoff,
            coverage=coverage,
            max_planned=max_planned,
        )
        if frame_error is not None:
            out["fallback"] = frame_error
            planner["status"] = "failed"
            break
        if validated is not None:
            break
        if attempt == 0:
            planner["repairs"] = 1
        else:
            out["fallback"] = "the program was invalid after one repair: " + "; ".join(errors[:4])
            planner["status"] = "invalid"

    planner["ms"] = int((clock() - began) * 1000)
    planner["tokens"] = _plan_token_total(frames)
    return validated


def _init_compiled_output(
    repo: deep.Repo,
    brief: Any,
    coverage: list[dict[str, str]],
    limits: tuple[int, int],
    retrieved: dict[str, Any],
    tests: dict[str, Any] | None,
    program: Any,
    plan_style: str,
) -> tuple[dict[str, Any], dict[str, Any]]:
    planner: dict[str, Any] = {
        "ms": 0,
        "tokens": 0,
        "repairs": 0,
        "status": "replayed" if program is not None else "ok",
        "style": "replay" if program is not None else plan_style,
        "cells": 0,
    }
    out: dict[str, Any] = {
        "findings": [],
        "dropped": [],
        "generic": [],
        "records": [],
        "stats": None,
        "assurance": [],
        "repo": repo,
        "brief": brief,
        "program": None,
        "fallback": None,
        "diff_cut": False,
        "planner": planner,
        "tests": {
            "enabled": tests is not None,
            "mechanism": None,
            "note": None,
            "runs": [],
            "env": "none",
            "toolchain": [],
        },
        "refuted": 0,
        "uncovered": [],
        "coverage": coverage,
        "limits": limits,
        "notes": [],
        "retrieval": {key: retrieved[key] for key in ("items", "chars", "ms")},
    }
    return out, planner


def _apply_started_tests(out: dict[str, Any], started: dict[str, Any]) -> None:
    out["tests"]["mechanism"] = started["mechanism"]
    out["tests"]["note"] = started["note"]
    out["tests"]["env"] = started["env"]
    out["tests"]["toolchain"] = started["toolchain"]
    out["findings"] = list(started["observed"])


def _build_diff_lines(files: list[FileDiff]) -> dict[str, set[int]]:
    return {
        item.path: {
            line.new
            for hunk in item.hunks
            for line in hunk.lines
            if line.new is not None
        }
        for item in files
    }


async def _resolve_validated_program(
    out: dict[str, Any],
    *,
    program: Any,
    frames: Any,
    views: list[str],
    tests_allowed: bool,
    plan_model: str | None,
    plan_thinking: str | None,
    cutoff: float | None,
    clock: Callable[[], float],
    coverage: list[dict[str, str]],
    max_planned: int,
    planner: dict[str, Any],
) -> Program | None:
    if program is not None:
        validated, errors = validate(program, coverage, max_planned)
        if validated is None:
            out["fallback"] = "the given program is invalid: " + "; ".join(errors[:4])
        return validated

    return await _plan_frame_program(
        out,
        frames=frames,
        views=views,
        tests_allowed=tests_allowed,
        plan_model=plan_model,
        plan_thinking=plan_thinking,
        cutoff=cutoff,
        clock=clock,
        coverage=coverage,
        max_planned=max_planned,
        planner=planner,
    )


async def _execute_compiled_body(
    out: dict[str, Any],
    ctx: _RunContext,
    started: dict[str, Any],
    session: Any,
) -> None:
    tests_allowed = session is not None and session.limit > len(session.records)
    runners, unavailable = runner_availability(session)
    runs_left = max(0, session.limit - len(session.records)) if session is not None else 0
    views, cut = planner_views(
        ctx.diff_text,
        ctx.brief.text,
        context=ctx.context,
        intent=ctx.intent,
        guidance=ctx.guidance,
        tests_block=deep.tests_block(started),
        runners=runners,
        tests_allowed=tests_allowed,
        coverage=ctx.coverage,
        limits=ctx.limits,
        retrieved=ctx.retrieved["text"],
        runs_left=runs_left,
    )
    out["diff_cut"] = cut

    if ctx.program is None and ctx.plan_style == "cell":
        sandbox = _resolve_cell_sandbox(ctx.cell_sandbox, started, ctx.tests)
        if sandbox is not None:
            await _run_cell_plan_mode(
                out,
                ctx,
                sandbox=sandbox,
                session=session,
                views=views,
                tests_allowed=tests_allowed,
                unavailable=unavailable,
            )
            return
        note = "the planner ran as one frame: no sandbox is available for planner cells"
        out["notes"] = [note]
        ctx.planner["style"] = "frame"

    validated = await _resolve_validated_program(
        out,
        program=ctx.program,
        frames=ctx.frames,
        views=views,
        tests_allowed=tests_allowed,
        plan_model=ctx.plan_model,
        plan_thinking=ctx.plan_thinking,
        cutoff=ctx.cutoff,
        clock=ctx.clock,
        coverage=ctx.coverage,
        max_planned=ctx.limits[0],
        planner=ctx.planner,
    )
    if validated is None:
        return

    out["program"] = validated.as_json()
    _dump_program(out["program"], ctx.dump_path)
    interpreter = _make_interpreter(validated, ctx, session, unavailable)
    await interpreter.run()
    _populate_compiled_output(
        out,
        interpreter,
        coverage=ctx.coverage,
        declared=validated.uncovered,
        planner=ctx.planner,
        summary=validated.summary,
        session=session,
    )


async def run_compiled(
    frames: Any,
    files: list[FileDiff],
    read_file: Callable[[str], list[str] | None],
    *,
    root: str,
    rev: str,
    diff_text: str,
    context: str,
    intent: str,
    guidance: str,
    plan_model: str | None,
    plan_thinking: str | None,
    ask_model: str | None,
    ask_thinking: str | None,
    cutoff: float | None,
    clock: Callable[[], float],
    cap: Callable[..., str],
    runner: Any = None,
    tests: dict[str, Any] | None = None,
    to_level: Callable[[Any], str | None] | None = None,
    title: str = "",
    description: str = "",
    base: str | None = None,
    enrich: Callable[[Any, dict[str, Any]], None] | None = None,
    generic: Callable[[dict[str, Any]], str | None] | None = None,
    program: Any = None,
    dump_path: str | None = None,
    plan_style: str = DEFAULT_PLAN_STYLE,
    plan_cells: int = DEFAULT_PLAN_CELLS,
    cell_sandbox: Any = "detect",
    python: str = "python3",
    knowledge: Any = None,
) -> dict[str, Any]:
    """Map, retrieve, plan (cells, frame, or replay), validate, and interpret."""
    repo = deep.Repo(root, rev, runner)
    if not repo.files():
        raise RuntimeError("the reviewed commit could not be read")
    repo.diff_by_file = deep.index_diff(diff_text)
    brief = deep.build_brief(
        repo, files, read_file, title=title, description=description, base=base, knowledge=knowledge
    )
    diff_lines = _build_diff_lines(files)
    limits = limits_for(sum(item.added + item.removed for item in files))
    coverage = coverage_items(brief, files)
    retrieved = retrieve(repo, brief, files, clock=clock)
    out, planner = _init_compiled_output(
        repo, brief, coverage, limits, retrieved, tests, program, plan_style
    )
    ctx = _RunContext(
        repo=repo, brief=brief, diff_text=diff_text, diff_lines=diff_lines,
        context=context, intent=intent, guidance=guidance, retrieved=retrieved,
        coverage=coverage, limits=limits, frames=frames, plan_model=plan_model,
        plan_thinking=plan_thinking, ask_model=ask_model, ask_thinking=ask_thinking,
        cutoff=cutoff, clock=clock, cap=cap, tests=tests, to_level=to_level,
        enrich=enrich, generic=generic, program=program, dump_path=dump_path,
        plan_style=plan_style, plan_cells=plan_cells, cell_sandbox=cell_sandbox,
        python=python, planner=planner,
    )

    session = None
    try:
        started = deep.start_tests(repo, files, tests, brief, root=root, rev=rev, clock=clock)
        session = started["session"]
        _apply_started_tests(out, started)
        await _execute_compiled_body(out, ctx, started, session)
    finally:
        if session is not None:
            session.close()
    return out
