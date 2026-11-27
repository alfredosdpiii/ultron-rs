"""`/review`: a finder-then-verifier code review over bounded inference frames.

The `/review` command asks the model to run one cell:

    import review_api
    review = await review_api.run(rlm, "<the command's arguments>")
    print(review.report)

1. Scope. The diff comes from git (working tree plus the branch since its merge base with the default branch, or
   since a given ref) or from `gh pr diff`. Files are parsed into hunks; lockfiles, generated, binary and deleted
   files are skipped and listed as not checked.
2. Chunks. Each file's hunks are rendered with new-file line numbers and unchanged code from the repository around
   them, packed into chunks of bounded size (a large hunk is split).
3. Find. One `rlm.map` over (reviewer x chunk): five specialists (correctness, security, architecture, tests,
   AI/LLM integration), each a frame with its own checklist and a findings contract. Frames, not sub-agents: a
   finder only needs the chunk it is given, and a frame costs one request where a sub-agent re-sends a system
   prompt and transcript every turn.
4. Dedupe. Findings on the same file, nearby lines and the same category (or the same claim) merge into one.
5. Verify. One `rlm.map` of verifier frames, one per finding, each seeing the cited source window, the hunk, and the
   definitions and uses of the names involved (found with `git grep`). Rejected findings are dropped; a confirmation
   whose evidence does not quote the source is downgraded to uncertain. A verifier that names code it lacks is
   asked again with it, up to three times.
   Sub-agents read past the diff where a fixed view cannot (a review of the whole local change, or `--deep`): one
   per changed file follows it into what it calls and what it replaces and raises findings for the same verifier,
   and one re-checks each blocker or major the verifier left undecided. They run on the host's sub-agent model.
6. Report. Grouped by severity, with the real source line quoted by code, counts, cost and what was not checked.

Everything runs under one token cap (`--budget`); frames past it come back `Incomplete` and are reported as
not checked. Nothing is posted unless `post(review, confirm=True)` is called after the user agreed.
"""
from __future__ import annotations

import os
import shutil
from dataclasses import dataclass as dataclass, field as field
from pathlib import Path as Path
from typing import Any, Callable

from infer_api import Budget as Budget, FrameError as FrameError, Incomplete as Incomplete
from review_prompts import (
    ALIASES as ALIASES,
    DEEP_VERIFY_BRIEF as DEEP_VERIFY_BRIEF,
    EXPLORE_BRIEF as EXPLORE_BRIEF,
    REVIEWERS as REVIEWERS,
    SWEEP_TASK as SWEEP_TASK,
    VERIFIER_TASK as VERIFIER_TASK,
    Reviewer as Reviewer,
    finder_task as finder_task,
)
from review_api_diff import (
    _reviewer_keys as _reviewer_keys,
    _untracked as _untracked,
    applies as applies,
    build_chunks as build_chunks,
    classify_targets as classify_targets,
    parse_args as parse_args,
    parse_diff as parse_diff,
    plan_find as plan_find,
    resolve_scope as resolve_scope,
)
from review_api_findings import (
    apply_verdicts as apply_verdicts,
    dedupe as dedupe,
    evidence_quotes_source as evidence_quotes_source,
    needed_code as needed_code,
    normalize_category as normalize_category,
    normalize_findings as normalize_findings,
    related_code as related_code,
    similar_claims as similar_claims,
    symbols_for as symbols_for,
)
from review_api_inventories import (
    _acceptance_items as _acceptance_items,
    _ancestor_dirs as _ancestor_dirs,
    _handler_answers as _handler_answers,
    _knowledge_rules as _knowledge_rules,
    _lessons as _lessons,
    _name_words as _name_words,
    _purposes as _purposes,
    _read_convention_file as _read_convention_file,
    _run_items as _run_items,
    acceptance_reviewer as acceptance_reviewer,
    ci_commands as ci_commands,
    contract_inventory as contract_inventory,
    contract_reviewer as contract_reviewer,
    conventions_inventory as conventions_inventory,
    conventions_reviewer as conventions_reviewer,
    keep_lesson as keep_lesson,
    lessons_reviewer as lessons_reviewer,
    out_of_diff_callers as out_of_diff_callers,
    recall_memorantado as recall_memorantado,
    recall_memorantado_items as recall_memorantado_items,
    reuse_inventory as reuse_inventory,
    reuse_reviewer as reuse_reviewer,
)
from review_api_report import (
    _finish as _finish,
    _plan_report as _plan_report,
    _quote as _quote,
    _save as _save,
    _write_receipt as _write_receipt,
    render_report as render_report,
    waive as waive,
)
from review_api_stages import (
    _POSTED as _POSTED,
    _build_reviewers_list,
    _children as _children,
    _collect_inventories,
    _deep_verify as _deep_verify,
    _empty_scope_review,
    _execute_review_plan,
    _explore_findings as _explore_findings,
    _explore_start as _explore_start,
    _gap_sweep as _gap_sweep,
    _init_review_run,
    _plan_review_budget,
    _run_find_stage as _run_find_stage,
    _run_sandboxed_tests as _run_sandboxed_tests,
    _run_static_stage as _run_static_stage,
    _run_verify_stage as _run_verify_stage,
    _saved_review_model as _saved_review_model,
    post as post,
)
from review_api_types import (
    ACCEPTANCE_KEY as ACCEPTANCE_KEY,
    AUTO_BUDGET_HEADROOM as AUTO_BUDGET_HEADROOM,
    AUTO_BUDGET_MAX as AUTO_BUDGET_MAX,
    CHILD_POLL_S as CHILD_POLL_S,
    CHUNK_CHARS as CHUNK_CHARS,
    CONTEXT_LINES as CONTEXT_LINES,
    CONTRACT_ANSWER_LINES as CONTRACT_ANSWER_LINES,
    CONTRACT_BODY_LINES as CONTRACT_BODY_LINES,
    CONTRACT_CALLER_LINES as CONTRACT_CALLER_LINES,
    CONTRACT_CALLERS as CONTRACT_CALLERS,
    CONTRACT_CHARS as CONTRACT_CHARS,
    CONTRACT_ITEMS as CONTRACT_ITEMS,
    CONTRACT_KEY as CONTRACT_KEY,
    CONTRACT_ROUTES as CONTRACT_ROUTES,
    CONVENTIONS_CHARS as CONVENTIONS_CHARS,
    CONVENTIONS_FILES as CONVENTIONS_FILES,
    CONVENTIONS_KEY as CONVENTIONS_KEY,
    DEDUPE_WINDOW as DEDUPE_WINDOW,
    DEEP_DEFAULT as DEEP_DEFAULT,
    DEEP_MAX as DEEP_MAX,
    DEEP_TIMEOUT_MS as DEEP_TIMEOUT_MS,
    DEFAULT_BUDGET_TOKENS as DEFAULT_BUDGET_TOKENS,
    DUPLICATE_QUESTION as DUPLICATE_QUESTION,
    EMPTY_TREE as EMPTY_TREE,
    EXPLORE_HUNK_CHARS as EXPLORE_HUNK_CHARS,
    EXPLORE_KEY as EXPLORE_KEY,
    EXPLORE_TIMEOUT_MS as EXPLORE_TIMEOUT_MS,
    EXPLORERS as EXPLORERS,
    FIND_SHARE as FIND_SHARE,
    FINDINGS_CONTRACT as FINDINGS_CONTRACT,
    FRAME_TIMEOUT_MS as FRAME_TIMEOUT_MS,
    GH_TIMEOUT_S as GH_TIMEOUT_S,
    GIT_TIMEOUT_S as GIT_TIMEOUT_S,
    LESSON_MARK as LESSON_MARK,
    LESSON_SITUATION as LESSON_SITUATION,
    LESSONS_KEY as LESSONS_KEY,
    LESSONS_QUERY as LESSONS_QUERY,
    MAX_FINDINGS_PER_FRAME as MAX_FINDINGS_PER_FRAME,
    MAX_LESSONS as MAX_LESSONS,
    MAX_LINE_CHARS as MAX_LINE_CHARS,
    MAX_UNTRACKED_BYTES as MAX_UNTRACKED_BYTES,
    MAX_UNTRACKED_FILES as MAX_UNTRACKED_FILES,
    MIN_BUDGET_TOKENS as MIN_BUDGET_TOKENS,
    NEED_CHARS as NEED_CHARS,
    NEED_LINES as NEED_LINES,
    NEED_ROUNDS as NEED_ROUNDS,
    NEEDS_CONTRACT as NEEDS_CONTRACT,
    PURPOSES_CHARS as PURPOSES_CHARS,
    PURPOSES_CONTRACT as PURPOSES_CONTRACT,
    PURPOSES_TASK as PURPOSES_TASK,
    RELATED_CHARS as RELATED_CHARS,
    REPORT_FINDINGS as REPORT_FINDINGS,
    REUSE_ALIKE as REUSE_ALIKE,
    REUSE_BODY_LINES as REUSE_BODY_LINES,
    REUSE_CHARS as REUSE_CHARS,
    REUSE_COMPAT_LINES as REUSE_COMPAT_LINES,
    REUSE_DEFINITIONS as REUSE_DEFINITIONS,
    REUSE_KEY as REUSE_KEY,
    SEVERITIES as SEVERITIES,
    SEVERITY_TITLES as SEVERITY_TITLES,
    SIZE_CEILING as SIZE_CEILING,
    SIZE_TARGET as SIZE_TARGET,
    SWEEP_CALLERS_CHARS as SWEEP_CALLERS_CHARS,
    SWEEP_DIFF_CHARS as SWEEP_DIFF_CHARS,
    SWEEP_KEY as SWEEP_KEY,
    USAGE as USAGE,
    VERDICT_CONTRACT as VERDICT_CONTRACT,
    VERIFY_WINDOW as VERIFY_WINDOW,
    _ANSWER as _ANSWER,
    _BINARY_EXT as _BINARY_EXT,
    _BODY_START as _BODY_START,
    _CALL as _CALL,
    _CATEGORY_WORDS as _CATEGORY_WORDS,
    _CI_RUN as _CI_RUN,
    _CI_SETUP as _CI_SETUP,
    _COMMON as _COMMON,
    _COMPAT as _COMPAT,
    _CONTRACT_ENTRY as _CONTRACT_ENTRY,
    _DATA as _DATA,
    _DEF as _DEF,
    _DEFINITION as _DEFINITION,
    _DEFINITION_KIND as _DEFINITION_KIND,
    _DEFINITION_WORDS as _DEFINITION_WORDS,
    _DIFF_GIT as _DIFF_GIT,
    _DOC as _DOC,
    _GENERATED as _GENERATED,
    _GENERIC_WORDS as _GENERIC_WORDS,
    _HANDLER as _HANDLER,
    _HUNK as _HUNK,
    _HUNK_LINE as _HUNK_LINE,
    _LLM_HINT as _LLM_HINT,
    _NEED_NAME as _NEED_NAME,
    _OPS_DOC as _OPS_DOC,
    _PREFIX as _PREFIX,
    _PR_URL as _PR_URL,
    _RAISE as _RAISE,
    _REVIEWER_CATEGORY as _REVIEWER_CATEGORY,
    _ROUTE as _ROUTE,
    _STOP as _STOP,
    _TEST as _TEST,
    _WINDOW_LINE as _WINDOW_LINE,
    Chunk as Chunk,
    DiffLine as DiffLine,
    FileDiff as FileDiff,
    Git as Git,
    Hunk as Hunk,
    Options as Options,
    Review as Review,
    ReviewError as ReviewError,
    Runner as Runner,
    Scope as Scope,
    Target as Target,
    _clip as _clip,
    _context as _context,
    _estimate_tokens as _estimate_tokens,
    _failure as _failure,
    _local_reader as _local_reader,
    _normalize as _normalize,
    _rank as _rank,
    _receipt_path as _receipt_path,
    _rev_reader as _rev_reader,
    _run_process as _run_process,
    _source_bodies as _source_bodies,
    _spent as _spent,
    _split_hunk as _split_hunk,
    _split_lines as _split_lines,
    _text as _text,
    _tokens as _tokens,
    _unquote as _unquote,
    _within as _within,
    _words as _words,
    added_file as added_file,
    default_branch as default_branch,
    file_kind as file_kind,
    hunk_for as hunk_for,
    parse_json_object as parse_json_object,
    render_hunk as render_hunk,
    skip_reason as skip_reason,
    source_window as source_window,
)


async def run(
    rlm: Any,
    args: str | list[str] | None = "",
    *,
    cwd: str | None = None,
    runner: Runner | None = None,
    which: Callable[[str], str | None] = shutil.which,
) -> Review:
    """Review the changes `args` selects (see USAGE) and return a Review; `print(review.report)` shows it."""
    init = await _init_review_run(rlm, args, cwd or os.getcwd(), runner, which)
    if isinstance(init, Review):
        return init
    options, scope = init
    git = Git(scope.root, runner)
    reviewers = await _build_reviewers_list(rlm, git, scope, options)
    chunks, skipped = build_chunks(scope.files, scope.read_file)
    not_checked = [*scope.notes, *(f"{path}: {reason}" for path, reason in skipped)]
    reviewers, reuse, contracts, conventions = _collect_inventories(
        git, scope, options, chunks, reviewers, not_checked
    )
    if not chunks:
        return _empty_scope_review(git, scope, options, skipped)
    find_budget, plan = _plan_review_budget(scope, options, chunks, reviewers)
    if options.plan:
        return Review(_plan_report(scope.label, chunks, plan, skipped, options), scope=scope.label, pr=scope.pr)
    return await _execute_review_plan(
        rlm, git, scope, options, reviewers, chunks, plan, reuse, contracts, conventions, find_budget, not_checked
    )
