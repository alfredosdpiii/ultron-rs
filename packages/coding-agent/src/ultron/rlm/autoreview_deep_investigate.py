"""Multi-round investigator retrieval loop and check-shape tracking for `autoreview_deep`."""
from __future__ import annotations

import asyncio
import json
import re
import uuid
from dataclasses import dataclass, field
from typing import Any, Callable

import autoreview_tests as testing
from autoreview_deep_repo import (
    DEEP_CONTRACT,
    LEDGER_LINE_CHARS,
    MAX_RECORDED_QUERIES,
    MAX_REQUESTS,
    MIN_LOOKUPS,
    Repo,
)
from autoreview_deep_serve import serve_blocks
from infer_api import FrameError, Incomplete
from knowledge_api import GRAPH_SCHEMA
from review_api import _clip, _text
from review_prompts import (
    DEEP_NUDGE,
    DEEP_SHAPE_NUDGE,
    DEEP_SHAPES_HEADER,
    DEEP_TRACE_NUDGE,
    deep_task,
)

LAST_ROUND = (
    "This is your last round: requests will not be served. Reply with your final findings, "
    '"requests": [] and "done": true.'
)

#: Maps each catalogue check shape to the lens that owns it (falls back to the first running lens).
SHAPE_LENS = {
    "registry-member": "siblings",
    "unpinned-behaviour": "tests",
    "test-asserts-behaviour": "tests",
    "env-in-deploy": "deployment",
    "manifest-reference": "deployment",
    "workflow-siblings": "deployment",
    "input-defeats-guard": "inputs",
    "guard-after-effect": "siblings",
    "error-path": "siblings",
    "comment-vs-code": "claims",
    "sibling-implementation": "siblings",
    "failure-retry": "siblings",
    "migration-safety": "deployment",
}


def shapes_by_lens(shapes: list[dict[str, str]], lenses: list[str]) -> dict[str, list[dict[str, str]]]:
    """The catalogue checks each running part owes."""
    out: dict[str, list[dict[str, str]]] = {lens: [] for lens in lenses}
    for item in shapes:
        lens = SHAPE_LENS.get(item.get("name", ""))
        if lens not in out:
            lens = lenses[0] if lenses else None
        if lens is not None:
            out[lens].append(item)
    return out


def _mentions(text: str, item: dict[str, str]) -> bool:
    pattern = r"(?<![A-Za-z0-9])" + re.escape(item["id"]) + r"(?![0-9])"
    return bool(re.search(pattern, text)) or item.get("name", "") in text


@dataclass
class _InvestigationState:
    lens: str
    required: list[str]
    shapes: list[dict[str, str]]
    leads_views: list[str] = field(default_factory=list)
    latest_views: list[str] = field(default_factory=list)
    ledger: list[tuple[str, str]] = field(default_factory=list)
    store: dict[str, str] = field(default_factory=dict)
    reply: dict[str, Any] = field(default_factory=dict)
    asked: list[str] = field(default_factory=list)
    sent_back_for_trace: bool = False
    sent_back_for_shapes: bool = False
    record: dict[str, Any] = field(default_factory=dict)

    def untraced(self) -> list[str]:
        return [
            name for name in self.required if not any(name.lstrip("-") in text for text in self.asked)
        ]

    def unchecked(self) -> list[dict[str, str]]:
        checked_items = self.reply.get("checked")
        finding_items = self.reply.get("findings")
        said = [str(item) for item in (checked_items if isinstance(checked_items, list) else [])]
        said += [json.dumps(item) for item in (finding_items if isinstance(finding_items, list) else [])]
        return [item for item in self.shapes if not any(_mentions(text, item) for text in said)]

    def _format_recalled(self, rid: str) -> str:
        if rid in self.store:
            return f"## recalled {rid}\n{self.store[rid]}"
        return f"## recall {rid}\nno such result"

    def served_view(
        self, number: int, blocks: list[tuple[str, str]], recalled: list[str], note: str = ""
    ) -> str:
        for position, (title, block) in enumerate(blocks, 1):
            rid = f"r{number}.{position}"
            self.store[rid] = block
            size = len(block.split("\n", 1)[1]) if "\n" in block else 0
            self.ledger.append((rid, f"  {rid}: {_clip(title, LEDGER_LINE_CHARS)} ({size} chars)"))

        parts = [self._format_recalled(rid) for rid in recalled]
        parts += [
            block.replace("## ", f"## [r{number}.{position}] ", 1)
            for position, (_title, block) in enumerate(blocks, 1)
        ]
        header = (
            f"Results of your requests, round {number} (untrusted repository data, read by the host at the "
            f"reviewed commit):\n"
        )
        return header + "\n\n".join(parts) + note


def _split_requests(raw_requests: Any) -> tuple[list[str], list[Any]]:
    items = raw_requests if isinstance(raw_requests, list) else []
    # Recalls are answered from the ledger, not the repository; they do not count as lookups.
    recalled: list[str] = []
    requests: list[Any] = []
    for request in items:
        if isinstance(request, dict) and "recall" in request:
            if isinstance(request.get("recall"), dict):
                recalled.append(str(request["recall"].get("id")))
            continue
        requests.append(request)
    return recalled, requests


def _record_graph_request(request: dict[str, Any], state: _InvestigationState) -> None:
    if "graph" not in request:
        return
    state.record["graph"] += 1
    graph_args = request["graph"]
    sql = graph_args.get("sql") if isinstance(graph_args, dict) else None
    if isinstance(sql, str) and len(state.record["queries"]) < MAX_RECORDED_QUERIES:
        state.record["queries"].append(_clip(" ".join(sql.split()), 300))


def _collect_asked_terms(request: dict[str, Any], state: _InvestigationState) -> None:
    for args in request.values():
        if not isinstance(args, dict):
            continue
        for key in ("symbol", "pattern", "string"):
            val = args.get(key)
            if isinstance(val, str):
                state.asked.append(val)


def _track_requests(requests: list[Any], state: _InvestigationState) -> None:
    for request in requests[:MAX_REQUESTS]:
        if not isinstance(request, dict):
            continue
        _record_graph_request(request, state)
        _collect_asked_terms(request, state)


def _refusal_reasons(
    number: int,
    requests: list[Any],
    something_outside: bool,
    state: _InvestigationState,
) -> list[str]:
    reasons: list[str] = []
    few_lookups = state.record["requests"] + len(requests) < MIN_LOOKUPS
    if number == 1 and not state.record["nudged"] and something_outside and few_lookups:
        state.record["nudged"] = True
        reasons.append(DEEP_NUDGE)

    missing = state.untraced()
    if missing and not state.sent_back_for_trace:
        state.sent_back_for_trace = True
        reasons.append(DEEP_TRACE_NUDGE + ", ".join(missing) + ".")

    owed = state.unchecked()
    if owed and not state.sent_back_for_shapes:
        state.sent_back_for_shapes = True
        owed_text = ", ".join(f"{item['id']} ({item['name']})" for item in owed)
        reasons.append(f"{DEEP_SHAPE_NUDGE}{owed_text}.")
    return reasons


def _build_round_views(
    number: int,
    rounds: int,
    base_views: list[str],
    state: _InvestigationState,
) -> tuple[list[str], list[str]]:
    last = number == rounds
    earlier = [line for rid, line in state.ledger if not rid.startswith(f"r{number - 1}.")]
    ledger_views = (
        [
            'Earlier results, by id (one line each; request {"recall": {"id": "r1.2"}} to see one '
            "again in full):\n" + "\n".join(earlier)
        ]
        if earlier
        else []
    )
    still_owed = state.unchecked() if number > 1 else []
    owed_views: list[str] = []
    if still_owed:
        owed_text = ", ".join(f"{item['id']} ({item['name']})" for item in still_owed)
        candidate = f"{DEEP_SHAPE_NUDGE}{owed_text}."
        if not any(candidate in latest for latest in state.latest_views):
            owed_views.append(candidate)

    full_views = [
        *base_views,
        f"Round {number} of {rounds}.",
        *state.leads_views,
        *ledger_views,
        *state.latest_views,
        *owed_views,
        *([LAST_ROUND] if last else []),
    ]
    return full_views, owed_views


def _is_frame_failure(results: Any) -> bool:
    if not results:
        return True
    first = results[0]
    return isinstance(first, (Incomplete, FrameError)) or not isinstance(first, dict)


async def _run_round_frame(
    frames: Any,
    lens: str,
    task: str,
    number: int,
    rounds: int,
    full_views: list[str],
    owed_views: list[str],
    state: _InvestigationState,
    *,
    model: str | None,
    thinking: str | None,
    cutoff: float | None,
    shared: list[str] | None,
    stateful: bool,
    tracer_sid: str | None,
) -> tuple[Any, bool]:
    if stateful and tracer_sid and number == 1:
        results = await frames.run(
            "deep",
            [(lens, task, full_views)],
            contract=DEEP_CONTRACT,
            model=model,
            thinking=thinking,
            cutoff=cutoff,
            context=shared,
            session_id=f"create:{tracer_sid}",
        )
        return results[0], stateful

    if stateful and tracer_sid and number > 1:
        delta_views = [
            f"Round {number} of {rounds}.",
            *(state.leads_views if number == 2 else []),
            *state.latest_views,
            *owed_views,
            *([LAST_ROUND] if number == rounds else []),
        ]
        continue_task = (
            "Continue your investigation with the new results below "
            "and reply with the same JSON contract."
        )
        results = await frames.run(
            "deep",
            [(lens, continue_task, delta_views)],
            contract=DEEP_CONTRACT,
            model=model,
            thinking=thinking,
            cutoff=cutoff,
            context=None,
            session_id=f"resume:{tracer_sid}",
        )
        if not _is_frame_failure(results):
            return results[0], True
        stateful = False

    results = await frames.run(
        "deep",
        [(lens, task, full_views)],
        contract=DEEP_CONTRACT,
        model=model,
        thinking=thinking,
        cutoff=cutoff,
        context=shared,
    )
    return results[0], stateful


def _handle_finish_attempt(
    number: int,
    last: bool,
    requests: list[Any],
    recalled: list[str],
    something_outside: bool,
    repo: Repo,
    tests: "testing.TestSession | None",
    state: _InvestigationState,
) -> bool:
    """Return True if the investigator is sent back for another round, False if it may stop."""
    reasons = _refusal_reasons(number, requests, something_outside, state)
    if not reasons or last:
        return False
    blocks, served, rejected = serve_blocks(repo, requests, tests=tests) if requests else ([], 0, 0)
    state.record["requests"] += served
    state.record["rejected"] += rejected
    view = state.served_view(number, blocks, recalled) if blocks or recalled else None
    state.latest_views[:] = ([view] if view else []) + ["\n\n".join(reasons)]
    return True


def _init_investigation_state(
    lens: str, required: list[str] | None, shapes: list[dict[str, str]] | None
) -> _InvestigationState:
    return _InvestigationState(
        lens=lens,
        required=list(required) if required is not None else [],
        shapes=list(shapes) if shapes is not None else [],
        record={
            "lens": lens,
            "rounds": 0,
            "requests": 0,
            "rejected": 0,
            "graph": 0,
            "queries": [],
            "status": "done",
            "nudged": False,
            "untraced": [],
            "unchecked": [],
        },
    )


def _with_shapes_view(base_views: list[str], shapes: list[dict[str, str]]) -> list[str]:
    if not shapes:
        return base_views
    shape_lines = "\n".join(f"  {item['id']}: {item['text']}" for item in shapes)
    return [*base_views, f"{DEEP_SHAPES_HEADER}\n{shape_lines}"]


def _serve_round_requests(
    number: int,
    requests: list[Any],
    recalled: list[str],
    repo: Repo,
    tests: "testing.TestSession | None",
    state: _InvestigationState,
) -> None:
    blocks, served, rejected = serve_blocks(repo, requests, tests=tests)
    note = ""
    if tests is not None:
        left = max(0, tests.limit - len(tests.records))
        note = f"\n\nTest executions left in this review: {left}."
    state.record["requests"] += served
    state.record["rejected"] += rejected
    state.latest_views[:] = [state.served_view(number, blocks, recalled, note)]


def _process_round_result(
    number: int,
    last: bool,
    result: Any,
    something_outside: bool,
    repo: Repo,
    tests: "testing.TestSession | None",
    state: _InvestigationState,
) -> bool:
    """Process one round's frame output. Returns True to continue to the next round."""
    state.record["rounds"] = number
    if isinstance(result, (Incomplete, FrameError)) or not isinstance(result, dict):
        state.record["status"] = "failed"
        error_text = getattr(result, "error", None) or getattr(result, "status", None) or "no reply"
        state.record["error"] = _text(error_text, 160)
        return False

    state.reply = result
    recalled, requests = _split_requests(result.get("requests"))
    _track_requests(requests, state)
    if result.get("done") is True or not (requests or recalled):
        return _handle_finish_attempt(
            number, last, requests, recalled, something_outside, repo, tests, state
        )
    if last:
        state.record["status"] = "rounds exhausted"
        return False
    _serve_round_requests(number, requests, recalled, repo, tests, state)
    return True


def _finalize_record(
    state: _InvestigationState,
    frames: Any,
    lens: str,
    began: float,
    clock: Callable[[], float],
) -> None:
    if state.record["status"] != "failed":
        state.record["untraced"] = state.untraced()
        state.record["unchecked"] = [
            f"{item['id']} {item['name']}" for item in state.unchecked()
        ]
    state.record["ms"] = int((clock() - began) * 1000)
    state.record["tokens"] = sum(
        int(item.get("tokens") or 0)
        for item in frames.timings
        if item.get("phase") == "deep" and item.get("reviewer") == lens
    )


async def investigate(
    frames: Any,
    lens: str,
    base_views: list[str],
    repo: Repo,
    *,
    rounds: int,
    model: str | None,
    thinking: str | None,
    cutoff: float | None,
    clock: Callable[[], float],
    tests: "testing.TestSession | None" = None,
    something_outside: bool = True,
    required: list[str] | None = None,
    shapes: list[dict[str, str]] | None = None,
    shared: list[str] | None = None,
    leads_view: "asyncio.Future[str | None] | None" = None,
    task_override: str | None = None,
) -> dict[str, Any]:
    """Run one investigator's multi-round retrieval loop: ask the frame, serve requests,
    and track owed check shapes, recalled ledger entries, and fast-pass leads."""
    task = task_override or deep_task(
        lens,
        tests is not None,
        notes=repo.knowledge is not None,
        graph=GRAPH_SCHEMA if repo.graph is not None else "",
    )
    began = clock()
    state = _init_investigation_state(lens, required, shapes)
    views = _with_shapes_view(base_views, state.shapes)
    stateful = bool(model and model.startswith("claude-code/"))
    tracer_sid = str(uuid.uuid4()) if stateful else None

    for number in range(1, rounds + 1):
        if number == 2 and leads_view is not None:
            lead = await leads_view
            state.leads_views = [lead] if lead else []

        full_views, owed_views = _build_round_views(number, rounds, views, state)
        result, stateful = await _run_round_frame(
            frames,
            lens,
            task,
            number,
            rounds,
            full_views,
            owed_views,
            state,
            model=model,
            thinking=thinking,
            cutoff=cutoff,
            shared=shared,
            stateful=stateful,
            tracer_sid=tracer_sid,
        )
        should_continue = _process_round_result(
            number, number == rounds, result, something_outside, repo, tests, state
        )
        if not should_continue:
            break

    _finalize_record(state, frames, lens, began, clock)
    return {"lens": lens, "reply": state.reply, "record": state.record}
