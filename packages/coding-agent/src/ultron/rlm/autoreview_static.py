"""Deterministic stage: plain-code checks of changed files before any model judges a finding.

Four stages, each degrading on its own (a missing tool or failure skips that stage, says why,
and leaves the others standing):

- `compile`: Python is compiled in-process, JSON and TOML are parsed; JS/Go/shell use tools.
- `lint`: `ruff` (when installed) for error-class and low-noise rules on added lines.
- `typecheck`: `mypy` (when installed) on the changed Python files alone.
- `loki`: Loki's core rule pack over each changed file, before and after (`review.loki`).

Tools run on copies of the reviewed commit's files in a private temporary directory with
configuration ignored (`ruff --isolated`, `mypy --config-file=`). Only problems on added lines
become findings. A stage that ran clean on a file contradicts a model's claim it settles.
"""
from __future__ import annotations

import re
import shutil
import tempfile
import time
from typing import Any, Callable

import autoreview_tests as testing
from autoreview_static_checks import (
    LOKI_BATCH_BYTES as LOKI_BATCH_BYTES,
    LOKI_NIT_RULES as LOKI_NIT_RULES,
    LOKI_SECURITY_RULES as LOKI_SECURITY_RULES,
    MAX_ORPHAN_FINDINGS as MAX_ORPHAN_FINDINGS,
    MAX_STAGE_FINDINGS as MAX_STAGE_FINDINGS,
    RUFF_NIT_RULES as RUFF_NIT_RULES,
    RUFF_RULES as RUFF_RULES,
    STAGES as STAGES,
    TOOL_TIMEOUT_S as TOOL_TIMEOUT_S,
    Executor as Executor,
    Which as Which,
    _broken as _broken,
    _broken_external_base as _broken_external_base,
    _finding as _finding,
    _loki_batches as _loki_batches,
    _new_lines as _new_lines,
    _parsing_python as _parsing_python,
    _Stage as _Stage,
    _text_of as _text_of,
    collect_static_inputs,
    find_orphaned_private_helpers,
    format_static_result,
    populate_workdir,
    run_compile_stage,
    run_lint_stage,
    run_loki_stage,
    run_sync_stage,
    run_typecheck_stage,
)
from review_api import FileDiff, Git, Scope

#: Claims a stage that ran clean on the file contradicts: (stage, pattern).
CONTRADICTED_BY = (
    (
        "compile",
        re.compile(
            r"\b(syntax ?error|indentation ?error|invalid syntax|parse error|unparse?able|"
            r"(does not|doesn't|will not|won't|fails? to|cannot|can't) (compile|parse|be parsed))\b",
            re.I,
        ),
    ),
    (
        "lint",
        re.compile(
            r"\b(name ?error|undefined (name|variable|function)|(is|are) not defined|"
            r"unbound (local|name)|unresolved (name|reference))\b",
            re.I,
        ),
    ),
    (
        "typecheck",
        re.compile(
            r"\b(type[- ]?check(ing|er)? (fails|error|errors)|mypy (error|fails|reports))\b",
            re.I,
        ),
    ),
)


def broken_js_base(
    path: str,
    base: str,
    node: str,
    workdir: str,
    executor: Executor,
) -> bool:
    """Whether the base commit's version of a JavaScript file failed `node --check` as well."""
    return _broken_external_base(
        path, base, [node, "--check"], workdir, executor, testing.sandbox_env
    )


def _compile(
    stage: _Stage,
    heads: dict[str, str],
    bases: dict[str, str | None],
    workdir: str,
    which: Which,
    executor: Executor,
) -> None:
    run_compile_stage(
        stage, heads, bases, workdir, which, executor, testing.sandbox_env
    )


def _lint(
    stage: _Stage,
    heads: dict[str, str],
    added: dict[str, set[int]],
    workdir: str,
    which: Which,
    executor: Executor,
) -> None:
    run_lint_stage(stage, heads, added, workdir, which, executor, testing.sandbox_env)


def _typecheck(
    stage: _Stage,
    heads: dict[str, str],
    added: dict[str, set[int]],
    workdir: str,
    which: Which,
    executor: Executor,
) -> None:
    run_typecheck_stage(stage, heads, added, workdir, which, executor, testing.sandbox_env)


async def _loki(
    stage: _Stage,
    rlm: Any,
    heads: dict[str, str],
    bases: dict[str, str | None],
    added: dict[str, set[int]],
) -> None:
    await run_loki_stage(stage, rlm, heads, bases, added)


async def run_static(
    rlm: Any,
    files: list[FileDiff],
    read_head: Callable[[str], list[str] | None],
    read_base: Callable[[str], list[str] | None],
    *,
    which: Which | None = None,
    executor: Executor | None = None,
) -> dict[str, Any]:
    """Run every stage over the changed files (not deleted, not binary)."""
    resolved_which = which or shutil.which
    resolved_executor = executor or testing.run_process
    env_fn = testing.sandbox_env
    heads, bases, added = collect_static_inputs(files, read_head, read_base)
    stages = {name: _Stage(name) for name in STAGES}
    if not heads:
        for stage in stages.values():
            stage.skip("no changed file could be read")
        return {"stages": [stage.record() for stage in stages.values()], "findings": [], "clean": {}}

    workdir = tempfile.mkdtemp(prefix="ultron-autoreview-static-")
    try:
        populate_workdir(workdir, heads)
        sync_runners: tuple[tuple[str, Callable[[_Stage], None]], ...] = (
            ("compile", lambda stage: run_compile_stage(
                stage, heads, bases, workdir, resolved_which, resolved_executor, env_fn
            )),
            ("lint", lambda stage: run_lint_stage(
                stage, heads, added, workdir, resolved_which, resolved_executor, env_fn
            )),
            ("typecheck", lambda stage: run_typecheck_stage(
                stage, heads, added, workdir, resolved_which, resolved_executor, env_fn
            )),
        )
        for name, runner in sync_runners:
            run_sync_stage(stages[name], runner)

        started = time.monotonic()
        try:
            await run_loki_stage(stages["loki"], rlm, heads, bases, added)
        except Exception as error:
            stages["loki"].fail(error)
        stages["loki"].ms = int((time.monotonic() - started) * 1000)
    finally:
        shutil.rmtree(workdir, ignore_errors=True)
    return format_static_result(stages)


def contradiction(finding: dict[str, Any], static: dict[str, Any] | None) -> str | None:
    """Why the deterministic stage contradicts a model's finding, or None."""
    if not static:
        return None

    file_path = finding.get("file")
    clean_stages = static["clean"].get(file_path if isinstance(file_path, str) else "")
    if not clean_stages:
        return None

    raw_claim = finding.get("claim")
    raw_why = finding.get("why")
    claim_text = raw_claim if isinstance(raw_claim, str) else ""
    why_text = raw_why if isinstance(raw_why, str) else ""
    text = f"{claim_text} {why_text}"
    for stage, pattern in CONTRADICTED_BY:
        if stage in clean_stages and pattern.search(text):
            return f"{stage} ran clean on {finding['file']}"
    return None


def notes(static: dict[str, Any]) -> list[str]:
    """`notChecked` lines for stages that failed."""
    return [
        f"The {stage['name']} check of the deterministic stage failed ({stage['note']})."
        for stage in static["stages"]
        if stage["status"] == "failed"
    ]


def orphaned_private_helpers(git: Git | None, scope: Scope) -> list[dict[str, Any]]:
    """Find private helpers whose call sites were removed and now have no callers."""
    return find_orphaned_private_helpers(git, scope)
