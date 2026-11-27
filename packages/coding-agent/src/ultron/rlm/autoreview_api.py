"""`ultron autoreview`: the review pipeline behind automated pull-request reviews.

The host (`src/ultron/autoreview/engine.ts`) runs one cell per review, with no root model involved:

    import autoreview_api
    await autoreview_api.run_file(rlm, "<spec.json>", "<result.json>")

The spec says what to review and the result is plain JSON; everything that touches GitHub (discovery, checkout,
posting) happens in the host. The pipeline is `/review`'s (review_api.py), with what an unattended review needs:

1. Scope. Either a local repository and two commits (`repoDir`, `base`, `head`: the offline entry, source read
   from the head commit), or a checked-out worktree plus the diff text the host fetched (`workDir`, `diffPath`),
   or the diff alone (no source: the review is then marked incomplete).
2. Context. The PR's title and description, CI status, the repository's guideline files and other people's
   review comments are given to every finder frame as one block of untrusted data.
3. Re-check. On a re-review, each finding this account posted earlier is checked against the new source:
   fixed, still present, or no longer applicable.
4. Levels are calibrated, because the verdict asks for changes from a level up (`blockAt`, medium by default):
   five levels (critical, high, medium, low, nit), one rubric for finder and verifier, a failing scenario per
   finding, and a verifier that rates the level itself and says whether the scenario really fails. Critical and
   high stay so only when it does (`final_level`). `severity` in the result is the level on the old
   four-name scale, kept for consumers that know only that one.
5. The deterministic stage first (autoreview_static: compile, lint, type-check and Loki over the changed files),
   each check degrading alone: what it finds on added lines is confirmed by the host; a model finding it
   contradicts (a syntax error in a file that compiles) is dropped before any verifier sees it.
   Find, dedupe, verify: as in `/review`, with a finder contract that adds a line range and an exact
   replacement. Small files are packed into one slice, so a small pull request costs one finder frame per
   reviewer instead of one per reviewer and file. Every frame is a request of its own, scheduled here: at most
   `concurrency` at once, and a rate limit or other transient provider error is retried twice with backoff. By
   default there is no token cap, no per-frame timeout and no deadline: the review waits for every frame. Each
   can be set: with a cap (`budget`), a frame gets a token grant out of it (what was really spent plus the grants
   of the frames in flight never exceeds it); with a deadline, unfinished finder passes are given up when it
   comes, what was found is verified, and the result is marked incomplete. A finding other people already raised
   (same file, nearby line, similar claim) is not verified or posted again; it is returned under `alsoRaised`.
   A confirmed finding is then put to the judge seat blind (autoreview_blind: the claim and the code only); one
   it refutes, after escalating the dispute to another model family, is posted as uncertain.
6. Result. Confirmed and uncertain findings, the most serious pre-existing problems (`preexisting`: real, but in
   code the change did not alter, so never inline and never blocking), what was dropped, timing, usage, what was
   not checked, whether coverage was complete, and the new-file line ranges of the diff (the host validates
   inline comments on them).

Frames have no tools. Everything a frame sees is data, never instructions.
"""
from __future__ import annotations

import asyncio
import json
import os
import random
import re
import tempfile
import time
from pathlib import Path
from typing import Any, Callable

import autoreview_blind as blind
import autoreview_compiled as compiled
import autoreview_deep as deep
import autoreview_static as static_checks
from autoreview_api_guides import (
    _AUTO_RISKY_TOKENS as _AUTO_RISKY_TOKENS,
    _FRAMEWORK as _FRAMEWORK,
    _KIND_ORDER as _KIND_ORDER,
    _LANGUAGES as _LANGUAGES,
    _OTHER_FILES as _OTHER_FILES,
    CI_CHARS as CI_CHARS,
    COMMENT_CHARS as COMMENT_CHARS,
    DEFAULT_THINKING as DEFAULT_THINKING,
    DESCRIPTION_CHARS as DESCRIPTION_CHARS,
    GUIDE_CHARS as GUIDE_CHARS,
    GUIDELINE_CHARS as GUIDELINE_CHARS,
    GUIDELINE_FILES as GUIDELINE_FILES,
    INTENT_CHARS as INTENT_CHARS,
    INTENT_TOTAL_CHARS as INTENT_TOTAL_CHARS,
    MAX_CONTEXT_COMMENTS as MAX_CONTEXT_COMMENTS,
    MAX_GUIDE_FILES as MAX_GUIDE_FILES,
    MERGE_ARCH_LINES as MERGE_ARCH_LINES,
    OTHERS_WINDOW as OTHERS_WINDOW,
    REVIEW_GUIDE_CHARS as REVIEW_GUIDE_CHARS,
    REVIEW_GUIDE_FILES as REVIEW_GUIDE_FILES,
    REVIEW_GUIDE_HEADER as REVIEW_GUIDE_HEADER,
    SINGLE_SLICE_LINES as SINGLE_SLICE_LINES,
    SLICE_SEPARATOR as SLICE_SEPARATOR,
    THINKING_LEVELS as THINKING_LEVELS,
    TITLE_CHARS as TITLE_CHARS,
    _bounded as _bounded,
    _number as _number,
    _pick_auto_mode as _pick_auto_mode,
    _read_text as _read_text,
    _thinking as _thinking,
    context_block as context_block,
    diff_line_ranges as diff_line_ranges,
    load_guides as load_guides,
    map_line as map_line,
    member_for as member_for,
    merged_reviewers as merged_reviewers,
    pack_chunks as pack_chunks,
    raised_by_others as raised_by_others,
    repo_review_guide as repo_review_guide,
    stated_intent as stated_intent,
)
from autoreview_api_pipeline import (
    BRIEF_RECORD_CHARS as BRIEF_RECORD_CHARS,
    DEADLINE_ERROR as DEADLINE_ERROR,
    DEFAULT_ASK_THINKING as DEFAULT_ASK_THINKING,
    DEFAULT_CONCURRENCY as DEFAULT_CONCURRENCY,
    DEFAULT_DEADLINE_S as DEFAULT_DEADLINE_S,
    DEFAULT_DEEP_THINKING as DEFAULT_DEEP_THINKING,
    DEFAULT_FRAME_TIMEOUT_S as DEFAULT_FRAME_TIMEOUT_S,
    DEFAULT_MODE as DEFAULT_MODE,
    DEFAULT_MODEL_CONCURRENCY as DEFAULT_MODEL_CONCURRENCY,
    DEFAULT_PLAN_THINKING as DEFAULT_PLAN_THINKING,
    DEFAULT_RETRY_BASE_S as DEFAULT_RETRY_BASE_S,
    FIND_DEADLINE_SHARE as FIND_DEADLINE_SHARE,
    MAX_CONCURRENCY as MAX_CONCURRENCY,
    MAX_MODEL_CONCURRENCY as MAX_MODEL_CONCURRENCY,
    MAX_PREEXISTING as MAX_PREEXISTING,
    MODES as MODES,
    OVERLAP_LINES as OVERLAP_LINES,
    UNLIMITED as UNLIMITED,
    ReviewPipeline,
    _build_test_options as _build_test_options,
    _collect_compiled_findings as _collect_compiled_findings,
    _knowledge_rules_view as _knowledge_rules_view,
    _parse_run_config as _parse_run_config,
    _prepare_deep_context as _prepare_deep_context,
)
from autoreview_api_recheck import (
    MAX_EARLIER as MAX_EARLIER,
    MAX_REPLIES as MAX_REPLIES,
    RECHECK_CONTRACT as RECHECK_CONTRACT,
    RECHECK_DIFF_CHARS as RECHECK_DIFF_CHARS,
    RECHECK_LINE_CHARS as RECHECK_LINE_CHARS,
    RECHECK_SHARE as RECHECK_SHARE,
    RECHECK_WINDOW as RECHECK_WINDOW,
    REPLY_CHARS as REPLY_CHARS,
    _elsewhere_text as _elsewhere_text,
    _hunks_text as _hunks_text,
    _recheck_change_views as _recheck_change_views,
    _run_recheck_stage as _run_recheck_stage,
    recheck_earlier as recheck_earlier,
)
from autoreview_api_rules import (
    _CITED as _CITED,
    AUTOREVIEW_FINDINGS_CONTRACT as AUTOREVIEW_FINDINGS_CONTRACT,
    AUTOREVIEW_VERDICT_BATCH_CONTRACT as AUTOREVIEW_VERDICT_BATCH_CONTRACT,
    AUTOREVIEW_VERDICT_CONTRACT as AUTOREVIEW_VERDICT_CONTRACT,
    IDENTICAL as IDENTICAL,
    LESSON_DEMOTE_BELOW as LESSON_DEMOTE_BELOW,
    LEVEL_TO_OLD as LEVEL_TO_OLD,
    LEVELS as LEVELS,
    MAX_REPLACEMENT_CHARS as MAX_REPLACEMENT_CHARS,
    MAX_REPLACEMENT_SPAN as MAX_REPLACEMENT_SPAN,
    MERGE_WINDOW as MERGE_WINDOW,
    NEVER_SERIOUS as NEVER_SERIOUS,
    OLD_TO_LEVEL as OLD_TO_LEVEL,
    SERIOUS as SERIOUS,
    SERIOUS_ONLY_IF_FAILS as SERIOUS_ONLY_IF_FAILS,
    UNPINNED_SCHEMA as UNPINNED_SCHEMA,
    _extras as _extras,
    _final_level as _final_level,
    _holds as _holds,
    _public as _public,
    apply_lessons as apply_lessons,
    capped_level as capped_level,
    check_view as check_view,
    dedupe_fast as dedupe_fast,
    duplicate_record as duplicate_record,
    final_level as final_level,
    finding_kind as finding_kind,
    generic_reason as generic_reason,
    group_root_causes as group_root_causes,
    level_rank as level_rank,
    nearly_identical as nearly_identical,
    parse_rules as parse_rules,
    set_level as set_level,
    to_level as to_level,
)
from autoreview_api_verify import (
    DEFAULT_VERIFY_BATCH as DEFAULT_VERIFY_BATCH,
    EARLIER_WINDOW as EARLIER_WINDOW,
    MAX_VERIFY_BATCH as MAX_VERIFY_BATCH,
    VERIFY_SOLO_CHARS as VERIFY_SOLO_CHARS,
    _build_verify_jobs as _build_verify_jobs,
    _build_verify_views as _build_verify_views,
    _filter_verify_candidates as _filter_verify_candidates,
    _run_verifier_stage as _run_verifier_stage,
    _summarize_hybrid_verification as _summarize_hybrid_verification,
    _unpack_verify_batch as _unpack_verify_batch,
)
from infer_api import Budget, FrameError, Incomplete
from knowledge_api import Knowledge as Knowledge
from review_api import (
    CHUNK_CHARS as CHUNK_CHARS,
    CONVENTIONS_KEY as CONVENTIONS_KEY,
    FIND_SHARE as FIND_SHARE,
    FINDINGS_CONTRACT as FINDINGS_CONTRACT,
    MAX_FINDINGS_PER_FRAME as MAX_FINDINGS_PER_FRAME,
    MIN_BUDGET_TOKENS as MIN_BUDGET_TOKENS,
    SWEEP_DIFF_CHARS as SWEEP_DIFF_CHARS,
    VERDICT_CONTRACT as VERDICT_CONTRACT,
    Chunk as Chunk,
    FileDiff as FileDiff,
    Git as Git,
    ReviewError as ReviewError,
    Runner as Runner,
    Scope as Scope,
    _clip as _clip,
    _estimate_tokens as _estimate_tokens,
    _failure as _failure,
    _local_reader as _local_reader,
    _rev_reader as _rev_reader,
    _spent as _spent,
    _text as _text,
    _words as _words,
    apply_verdicts as apply_verdicts,
    build_chunks as build_chunks,
    conventions_inventory as conventions_inventory,
    conventions_reviewer as conventions_reviewer,
    evidence_quotes_source as evidence_quotes_source,
    file_kind as file_kind,
    hunk_for as hunk_for,
    needed_code as needed_code,
    normalize_findings as normalize_findings,
    out_of_diff_callers as out_of_diff_callers,
    parse_diff as parse_diff,
    plan_find as plan_find,
    recall_memorantado as recall_memorantado,
    related_code as related_code,
    render_hunk as render_hunk,
    similar_claims as similar_claims,
    skip_reason as skip_reason,
    source_window as source_window,
)
from review_prompts import (
    ALIASES as ALIASES,
    AUTOREVIEW_SWEEP_TASK as AUTOREVIEW_SWEEP_TASK,
    AUTOREVIEW_VERIFIER_BATCH_TASK as AUTOREVIEW_VERIFIER_BATCH_TASK,
    AUTOREVIEW_VERIFIER_TASK as AUTOREVIEW_VERIFIER_TASK,
    DOC_SWEEP_TASK as DOC_SWEEP_TASK,
    GUIDANCE_HEADER as GUIDANCE_HEADER,
    MINIMAL_FINDER_TASK as MINIMAL_FINDER_TASK,
    RECHECK_TASK as RECHECK_TASK,
    REVIEWERS as REVIEWERS,
    Reviewer as Reviewer,
    autoreview_finder_task as autoreview_finder_task,
)

# Re-export sibling modules for callers or tests importing them through autoreview_api.
blind = blind
compiled = compiled
deep = deep
static_checks = static_checks

HOST_MAX_TIMEOUT_MS = 60 * 60 * 1000
MIN_START_S = 3.0
FRAME_GRANT = 32_000
MIN_FRAME_OUTPUT = 2_000
MAX_RETRIES = 2
MAX_RETRY_WAIT_S = 30.0
AUTH_RETRY_S = 8.0


def recipe_for_model(model: str | None, mode: str) -> str:
    """Resolve the review recipe for `(model, mode)`. Currently honors `mode` directly; centralized here so
    model-family routing policies (e.g. `minimal` on frontier models after eval) live in one place."""
    _ = model
    return mode


def _reviewer_keys(only: Any) -> list[str]:
    from review_shared import parse_reviewer_keys

    return parse_reviewer_keys(only, reviewers=REVIEWERS, aliases=ALIASES, error_cls=ReviewError)


class _Usage:
    def __init__(self) -> None:
        self.frames = 0
        self.tokens = 0
        self.input = 0
        self.output = 0
        self.cost = 0.0
        self.by_phase: dict[str, dict[str, int]] = {}

    def add(self, results: Any, phase: str | None = None) -> None:
        calls, tokens = _spent(results)
        self.frames += calls
        self.tokens += tokens
        if phase:
            entry = self.by_phase.setdefault(phase, {"frames": 0, "tokens": 0})
            entry["frames"] += calls
            entry["tokens"] += tokens
        usage = getattr(results, "usage", None)
        if not isinstance(usage, dict):
            usage = {}
        self.input += int(usage.get("input_tokens") or 0)
        self.output += int(usage.get("output_tokens") or 0)
        self.cost += float(usage.get("cost") or 0)


_TRANSIENT = re.compile(
    r"\b(408|409|425|429|500|502|503|504|529)\b|rate.?lim|too many requests|overload|temporar"
    r"|unavailable|timed? ?out|timeout|econnreset|etimedout|epipe|socket hang up|fetch failed"
    r"|network|connection (reset|closed)",
    re.I,
)
_AUTH = re.compile(
    r"not logged in|failed to authenticate|session expired|unauthori[sz]ed|\b401\b|auth login",
    re.I,
)
_RETRY_AFTER = re.compile(
    r"retry[-_ ]?after\D{0,12}(\d+(?:\.\d+)?)|try again in (\d+(?:\.\d+)?) ?s"
    r"|reset[-_ ]?(?:seconds|in|after)\D{0,6}(\d+(?:\.\d+)?)",
    re.I,
)
_THROTTLE = re.compile(
    r"\b429\b|rate.?limit|too many requests|cooldown|model_cooldown|reset_seconds|overloaded",
    re.I,
)


def is_transient(result: Any) -> bool:
    """A frame failure worth another try: a rate limit, a timeout, an overloaded or unreachable provider, or a
    provider whose credentials are being refreshed."""
    return isinstance(result, FrameError) and result.error != DEADLINE_ERROR and bool(
        _TRANSIENT.search(result.error)
        or _AUTH.search(result.error)
        or _THROTTLE.search(result.error)
    )


def is_auth_failure(result: Any) -> bool:
    return isinstance(result, FrameError) and bool(_AUTH.search(result.error))


def is_throttled(result: Any) -> bool:
    return isinstance(result, FrameError) and bool(_THROTTLE.search(result.error))


def retry_after(error: str) -> float | None:
    """Seconds the provider asked to wait, when its error says so."""
    match = _RETRY_AFTER.search(error)
    return float(match.group(1) or match.group(2) or match.group(3)) if match else None


def _item_chars(item: Any) -> int:
    return len(item) if isinstance(item, str) else sum(len(part) for part in item)


class Frames:
    """Runs the review's frames, one request each: bounded concurrency, a per-frame timeout, a token grant out
    of the review's cap, retries for transient failures, and a record of how each frame went."""

    def __init__(
        self,
        rlm: Any,
        *,
        cap: int | None,
        usage: _Usage,
        concurrency: int = DEFAULT_CONCURRENCY,
        frame_timeout_s: float = DEFAULT_FRAME_TIMEOUT_S,
        retry_base_s: float = DEFAULT_RETRY_BASE_S,
        clock: Callable[[], float] = time.monotonic,
        sleep: Callable[[float], Any] = asyncio.sleep,
        rng: Callable[[], float] = random.random,
        model_concurrency: dict[str, int] | None = None,
        system_prefix: bool = False,
    ) -> None:
        self.rlm = rlm
        self.cap = cap
        self.usage = usage
        self.system_prefix = system_prefix
        self.concurrency = max(1, min(MAX_CONCURRENCY, concurrency))

        limits = model_concurrency if model_concurrency is not None else {}
        self.model_limits: dict[str, int] = {
            name: max(1, min(MAX_MODEL_CONCURRENCY, int(limit)))
            for name, limit in limits.items()
        }
        self.in_flight: dict[str, int] = {}
        self.throttled: dict[str, int] = {}
        self._changed: asyncio.Condition | None = None

        self.frame_timeout_s = frame_timeout_s
        self.retry_base_s = retry_base_s
        self.clock = clock
        self.sleep = sleep
        self.rng = rng

        self.held = 0
        self.timings: list[dict[str, Any]] = []
        self._slots: asyncio.Semaphore | None = None

    async def run(
        self,
        phase: str,
        jobs: list[tuple[str, str, Any]],
        *,
        contract: Any,
        model: str | None,
        thinking: str | None,
        context: Any = None,
        cutoff: float | None = None,
        contracts: list[Any] | None = None,
        models: list[str] | None = None,
        session_id: str | None = None,
    ) -> list[Any]:
        """Run `jobs` ((label, task, item) each) and return their results in order."""
        if self._slots is None:
            self._slots = asyncio.Semaphore(self.concurrency)

        coros = [
            self._one(
                phase,
                label,
                task,
                item,
                contract=contracts[index] if contracts else contract,
                model=models[index] if models else model,
                thinking=thinking,
                context=context,
                cutoff=cutoff,
                session_id=session_id,
            )
            for index, (label, task, item) in enumerate(jobs)
        ]
        return list(await asyncio.gather(*coros))

    def limit_of(self, model: str | None) -> int:
        return self.model_limits.get(model or "", DEFAULT_MODEL_CONCURRENCY)

    async def _admit(self, model: str | None) -> None:
        """Wait until the model has a free slot under its in-flight limit."""
        if self._changed is None:
            self._changed = asyncio.Condition()
        key = model or ""
        async with self._changed:
            while self.in_flight.get(key, 0) >= self.limit_of(model):
                await self._changed.wait()
            self.in_flight[key] = self.in_flight.get(key, 0) + 1

    async def _release(self, model: str | None) -> None:
        assert self._changed is not None
        key = model or ""
        async with self._changed:
            self.in_flight[key] = max(0, self.in_flight.get(key, 0) - 1)
            self._changed.notify_all()

    def _throttle(self, model: str | None) -> None:
        """The provider throttled this model: halve its in-flight limit for the rest of the review."""
        key = model or ""
        limit = max(1, self.limit_of(model) // 2)
        self.model_limits[key] = limit
        self.throttled[key] = limit

    def _allocate_grant(self, estimate: int) -> tuple[int, Any | None]:
        if self.cap is None:
            return 0, None
        grant = min(estimate + FRAME_GRANT, self.cap - self.usage.tokens - self.held)
        if grant < estimate + MIN_FRAME_OUTPUT:
            err = Incomplete(
                {"reason": "budget_exhausted", "detail": "the review's token cap is spent"}
            )
            return 0, err
        return grant, None

    def _frame_timeout(self, left: float | None) -> tuple[float, bool]:
        own = self.frame_timeout_s if self.frame_timeout_s > 0 else None
        candidates = [value for value in (own, left, HOST_MAX_TIMEOUT_MS / 1000) if value is not None]
        timeout_s = min(candidates)
        at_cutoff = left is not None and (own is None or left <= own)
        return timeout_s, at_cutoff

    async def _invoke_once(
        self,
        phase: str,
        task: str,
        item: Any,
        *,
        grant: int,
        timeout_s: float,
        contract: Any,
        model: str | None,
        thinking: str | None,
        context: Any,
        session_id: str | None,
    ) -> tuple[Any, int]:
        self.held += grant
        try:
            results = await self.rlm.map(
                [task],
                [item],
                context=context or None,
                contract=contract,
                budget=None if self.cap is None else Budget(tokens=grant),
                model=model,
                thinking=thinking,
                shared_prefix=self.system_prefix and bool(context),
                concurrency=1,
                timeout_ms=max(1_000, int(timeout_s * 1000)),
                session_id=session_id,
            )
            self.usage.add(results, phase)
            spent_tokens = _spent(results)[1]
            result = results[0] if len(results) else FrameError({"error": "the frame returned nothing"})
            return result, spent_tokens
        except Exception as error:
            return FrameError({"error": f"{type(error).__name__}: {error}"}), 0
        finally:
            self.held -= grant

    def _classify_frame_error(
        self, result: FrameError, timeout_s: float, at_cutoff: bool, model: str | None
    ) -> tuple[FrameError, str, bool]:
        if result.error == "cancelled":
            if at_cutoff:
                return FrameError({"error": DEADLINE_ERROR}), "deadline", True
            updated = FrameError({"error": f"timed out after {timeout_s:.0f} s"})
            status = "timeout"
        else:
            updated = result
            status = "failed"
        if is_throttled(updated):
            self._throttle(model)
        return updated, status, False

    def _retry_delay(self, result: FrameError, retries: int) -> float:
        hint = retry_after(result.error)
        if hint is not None:
            return min(MAX_RETRY_WAIT_S, hint)
        base = AUTH_RETRY_S if is_auth_failure(result) and self.retry_base_s > 0 else self.retry_base_s
        return min(MAX_RETRY_WAIT_S, base * (2 ** retries) * (0.5 + self.rng()))

    def _record_timing(
        self,
        phase: str,
        label: str,
        began: float,
        status: str,
        retries: int,
        tokens: int,
        model: str | None,
        result: Any,
    ) -> None:
        entry: dict[str, Any] = {
            "phase": phase,
            "reviewer": label,
            "ms": int((self.clock() - began) * 1000),
            "status": status,
            "retries": retries,
            "tokens": tokens,
        }
        if model:
            entry["model"] = model
        if isinstance(result, FrameError):
            entry["error"] = _text(result.error, 200)
        self.timings.append(entry)

    def _preflight_frame(
        self, cutoff: float | None, estimate: int
    ) -> tuple[float | None, int, Any | None, str]:
        left = None if cutoff is None else cutoff - self.clock()
        if left is not None and left < MIN_START_S:
            return left, 0, FrameError({"error": DEADLINE_ERROR}), "deadline"
        grant, budget_error = self._allocate_grant(estimate)
        if budget_error is not None:
            return left, 0, budget_error, "budget"
        return left, grant, None, "ok"

    def _compute_retry_wait(
        self, result: FrameError, retries: int, cutoff: float | None
    ) -> float | None:
        if not is_transient(result) or retries >= MAX_RETRIES:
            return None
        delay = self._retry_delay(result, retries)
        if cutoff is not None and self.clock() + delay + MIN_START_S >= cutoff:
            return None
        return delay

    async def _run_with_retries(
        self,
        phase: str,
        task: str,
        item: Any,
        *,
        estimate: int,
        contract: Any,
        model: str | None,
        thinking: str | None,
        context: Any,
        cutoff: float | None,
        session_id: str | None,
    ) -> tuple[Any, str, int, int]:
        retries = 0
        tokens = 0
        while True:
            left, grant, preflight_error, preflight_status = self._preflight_frame(cutoff, estimate)
            if preflight_error is not None:
                return preflight_error, preflight_status, retries, tokens

            timeout_s, at_cutoff = self._frame_timeout(left)
            result, spent = await self._invoke_once(
                phase,
                task,
                item,
                grant=grant,
                timeout_s=timeout_s,
                contract=contract,
                model=model,
                thinking=thinking,
                context=context,
                session_id=session_id,
            )
            tokens += spent
            if isinstance(result, Incomplete):
                return result, "incomplete", retries, tokens
            if not isinstance(result, FrameError):
                return result, "ok", retries, tokens

            result, status, stop = self._classify_frame_error(result, timeout_s, at_cutoff, model)
            delay = None if stop else self._compute_retry_wait(result, retries, cutoff)
            if delay is None:
                return result, status, retries, tokens
            retries += 1
            await self.sleep(delay)

    async def _execute_admitted_frame(
        self,
        phase: str,
        label: str,
        task: str,
        item: Any,
        *,
        estimate: int,
        contract: Any,
        model: str | None,
        thinking: str | None,
        context: str | None,
        cutoff: float | None,
        session_id: str | None,
    ) -> Any:
        began = self.clock()
        result, status, retries, tokens = await self._run_with_retries(
            phase,
            task,
            item,
            estimate=estimate,
            contract=contract,
            model=model,
            thinking=thinking,
            context=context,
            cutoff=cutoff,
            session_id=session_id,
        )
        self._record_timing(phase, label, began, status, retries, tokens, model, result)
        return result

    async def _one(
        self,
        phase: str,
        label: str,
        task: str,
        item: Any,
        *,
        contract: Any,
        model: str | None,
        thinking: str | None,
        context: str | None,
        cutoff: float | None,
        session_id: str | None = None,
    ) -> Any:
        estimate = (len(task) + _item_chars(item) + _item_chars(context or "")) // 3
        assert self._slots is not None
        async with self._slots:
            await self._admit(model)
            try:
                return await self._execute_admitted_frame(
                    phase,
                    label,
                    task,
                    item,
                    estimate=estimate,
                    contract=contract,
                    model=model,
                    thinking=thinking,
                    context=context,
                    cutoff=cutoff,
                    session_id=session_id,
                )
            finally:
                await self._release(model)


def describe_failure(result: Any) -> str | None:
    """Why a frame produced nothing, for the list of what was not checked; None for a value."""
    if isinstance(result, Incomplete):
        return f"ran out ({result.status})"
    if isinstance(result, FrameError):
        if result.error == DEADLINE_ERROR:
            return "were not finished at the review deadline"
        return f"failed ({_text(result.error, 120)})"
    return None


def _scope(spec: dict[str, Any], runner: Runner | None) -> tuple[Scope, Git | None, list[str]]:
    """The files to review, how to read their source, and reasons the coverage is incomplete."""
    incomplete: list[str] = []
    repo = spec.get("repoDir")
    work = spec.get("workDir")
    if repo:
        git = Git(repo, runner)
        if not git.ok("rev-parse", "--git-dir"):
            raise ReviewError(f"{repo} is not a git repository")
        base, head = spec.get("base"), spec.get("head")
        for name, rev in (("base", base), ("head", head)):
            if (
                not isinstance(rev, str)
                or not rev
                or rev.startswith("-")
                or not git.ok("rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}")
            ):
                raise ReviewError(f"{name} {rev!r} is not a commit in {repo}")
        diff = git.out(
            "-c",
            "diff.noprefix=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "-M",
            "-U3",
            base,
            head,
            "--",
        )
        scope = Scope(f"{base[:8]}..{head[:8]}", parse_diff(diff), _rev_reader(git, head), repo, head)
        return scope, git, incomplete

    files = parse_diff(_read_text(spec.get("diffPath")))
    label = str(spec.get("label") or "pull request")
    if work:
        return Scope(label, files, _local_reader(work), work, None), Git(work, runner), incomplete
    incomplete.append(
        "the repository could not be checked out, so only the diff was read (no surrounding source, no callers)"
    )
    return Scope(label, files, lambda _path: None, tempfile.gettempdir(), ""), None, incomplete


async def run(
    rlm: Any,
    spec: dict[str, Any],
    *,
    runner: Runner | None = None,
    clock: Callable[[], float] = time.monotonic,
    sleep: Callable[[float], Any] = asyncio.sleep,
    rng: Callable[[], float] = random.random,
) -> dict[str, Any]:
    """Review what `spec` describes and return the result (see the module docstring)."""
    pipeline = ReviewPipeline(
        rlm,
        spec,
        runner=runner,
        clock=clock,
        sleep=sleep,
        rng=rng,
        scope_fn=_scope,
        reviewer_keys_fn=_reviewer_keys,
        recipe_fn=recipe_for_model,
        frames_cls=Frames,
        usage_cls=_Usage,
        describe_failure_fn=describe_failure,
        is_throttled_fn=is_throttled,
    )
    return await pipeline.execute()


async def run_file(rlm: Any, spec_path: str, result_path: str) -> str:
    """Run the review in `spec_path` and write the result (or `{"error": ...}`) to `result_path`."""
    try:
        spec = json.loads(Path(spec_path).read_text(encoding="utf-8"))
        result = await run(rlm, spec)
    except ReviewError as error:
        result = {"error": str(error)}
    temporary = f"{result_path}.tmp"
    Path(temporary).write_text(json.dumps(result), encoding="utf-8")
    os.replace(temporary, result_path)
    return "error" if "error" in result else "ok"
