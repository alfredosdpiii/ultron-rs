"""Interpreter execution engine for compiled autoreview programs."""
from __future__ import annotations

import json
import re
from typing import Any, Callable

import autoreview_deep as deep
import autoreview_tests as testing
from autoreview_compiled_plan import (
    ANSWERS,
    ASK_CONTEXT_CHARS,
    ASK_CONTRACT,
    CAPPED,
    EVIDENCE_CHARS,
    LARGE_PROGRAM_STEPS,
    LEVELS,
    LOOKUPS,
    MAX_ASKS,
    MAX_HOLDS,
    MAX_PROGRAM_STEPS,
    MAX_STEPS,
    RECORD_CHARS,
    RESULT_CHARS,
    TEST_OPS,
    Program,
    _assert_leaves,
    _count_of,
    _count_predicate,
    _flat,
    _items_of,
    _PLACEHOLDER,
    _references,
    _render,
    as_dict,
    as_list,
    map_legacy_level,
    map_legacy_severity,
)
from infer_api import FrameError, Incomplete
from review_api import _clip, _text, normalize_category, source_window
from review_prompts import ASK_TASK, RESOLVE_TASK

_RESULT_SCALAR_FIELDS = frozenset(
    {"count", "status", "answer", "quote", "why", "title", "value"}
)
CapFn = Callable[..., str]
LevelFn = Callable[[Any], str | None]


def _plural_suffix(count: int) -> str:
    return "" if count == 1 else "s"


def _error_reason(reply: Any) -> str:
    error = getattr(reply, "error", None)
    if error:
        return str(error)
    status = getattr(reply, "status", None)
    return str(status) if status else "no reply"


def _summarize_assert(result: dict[str, Any]) -> str:
    value = result.get("value")
    if value:
        text = "holds"
    elif value is None:
        text = "unknown"
    else:
        text = "does not hold"

    if result.get("detail") and not value:
        text += f" ({result['detail']})"
    if result.get("contradicted"):
        text += "; contradicts the expectation"
    return text


def _summarize_finding(result: dict[str, Any]) -> str:
    if result.get("emitted"):
        return "finding emitted"
    gate = result.get("gate")
    if gate:
        return str(gate)
    detail = result.get("detail")
    return str(detail) if detail else "no finding"


def _summarize_lookup(op_name: str, result: dict[str, Any]) -> str:
    title = result.get("title", op_name)
    items = result.get("items")
    if not items:
        return title
    shown = "; ".join(_render(item) for item in items[:5])
    suffix = " ..." if len(items) > 5 else ""
    return f"{title}: {shown}{suffix}"


def _clone_template_step(
    sub: dict[str, Any],
    rename: dict[str, str],
    scope: dict[str, Any],
    origin: str,
) -> dict[str, Any]:
    clone = json.loads(json.dumps(sub))
    clone["id"] = rename[sub["id"]]
    clone["needs"] = [rename.get(name, name) for name in as_list(clone.get("needs"))]
    if clone.get("when"):
        clone["when"] = {
            "step": rename.get(clone["when"]["step"], clone["when"]["step"]),
            "not": clone["when"]["not"],
        }
    for key in ("over", "step"):
        if isinstance(clone.get(key), str):
            clone[key] = rename.get(clone[key], clone[key])
    for key in ("context", "all", "any", "evidence"):
        if clone.get(key):
            clone[key] = [rename.get(name, name) for name in clone[key]]
    clone["scope"] = scope
    clone["origin"] = origin
    return clone


def _eval_compound_truth(is_all: bool, values: list[bool | None]) -> bool | None:
    if is_all:
        if any(item is False for item in values):
            return False
        return True if all(item is True for item in values) else None
    if any(item is True for item in values):
        return True
    return False if all(item is False for item in values) else None


def _eval_word_predicate(
    kind: str,
    op: str,
    operand: str,
    target: dict[str, Any],
) -> tuple[bool | None, str]:
    actual = target.get("outcome") if kind == "status" else target.get("answer")
    value: bool | None = (actual == operand) if op == "==" else (actual != operand)
    if kind == "answer" and actual == "unclear" and not (op == "==" and operand == "unclear"):
        value = None
    return value, f"{kind} is {actual}"


def _format_check_description(check: dict[str, Any]) -> str:
    predicate = check.get("predicate")
    if predicate:
        return predicate
    if check.get("all"):
        return "all of " + ", ".join(as_list(check.get("all")))
    return "any of " + ", ".join(as_list(check.get("any")))


def _format_single_check_line(
    sid: str,
    check: dict[str, Any],
    result: dict[str, Any],
) -> str:
    what = _format_check_description(check)
    raw_value = result.get("value")
    came = "unknown" if raw_value is None else str(raw_value).lower()
    step_name = check.get("step") or "-"
    expected = str(check.get("expect")).lower()
    detail = result.get("detail", "")
    line = f"{sid}: {what} on step {step_name}; expected {expected}, came out {came} ({detail})"
    if check.get("holds"):
        line += f"; the planner wrote: {check.get('holds')}"
    return line


def _format_resolution_note(resolution: dict[str, Any]) -> tuple[str, str]:
    quote = resolution.get("quote") or ""
    why = resolution.get("why") or ""
    note = f"{resolution['id']}: {resolution['question']} -> {resolution['answer']}"
    if quote:
        note += f", quoting `{quote}`"
    if why:
        note += f" ({why})"

    part = (
        f"[{resolution['id']}] the check was undecided or contradicted; "
        "the small model answered yes"
    )
    if quote:
        part += f" quoting `{_clip(quote, 160)}`"
    return note, part


def _count_assert_outcomes(asserts: list[dict[str, Any]]) -> dict[str, int]:
    return {
        "held": sum(1 for item in asserts if item.get("value") is True),
        "failed": sum(1 for item in asserts if item.get("value") is False),
        "unknown": sum(1 for item in asserts if item.get("value") is None),
        "contradicted": sum(1 for item in asserts if item.get("contradicted")),
    }


def _format_holds_sentences(held: list[dict[str, Any]]) -> list[str]:
    sentences: list[str] = []
    for result in held:
        text = _text(result.get("holds"), 1000)
        if not text:
            continue
        if len(sentences) >= MAX_HOLDS:
            break
        ending = "" if text.endswith((".", "!", "?", "…")) else "."
        sentences.append(text + ending)
    return sentences


def _find_test_record(session: Any, title: str) -> dict[str, Any] | None:
    tokens = title.split()
    if not tokens or not tokens[-1].isdigit():
        return None
    number = int(tokens[-1])
    return next((item for item in session.records if item["n"] == number), None)


def _record_non_emitted_payload(gate: str, detail: str) -> dict[str, Any]:
    return {
        "op": "finding",
        "status": "ok",
        "emitted": False,
        "text": "",
        "gate": gate,
        "detail": detail,
    }


def _format_resolve_detail(why: str, answer: str, detail: str, quote: str) -> str:
    text = f"its check came out {why}; the small model answered {answer}"
    if detail:
        text += f" ({detail})"
    if quote:
        text += f", quoting `{_clip(quote, 160)}`"
    return text


def _build_finding_dict(
    step: dict[str, Any],
    *,
    path: str,
    line: int,
    level: str,
    stated: str,
    category: str,
    claim: str,
    why: str,
    scenario: str,
    suggested_fix: str,
    citations: list[dict[str, Any]],
    beyond: bool,
    ev: dict[str, Any],
) -> dict[str, Any]:
    return {
        "file": path,
        "line": line,
        "level": level,
        "severity": map_legacy_severity(level),
        "finder_level": stated,
        "category": category,
        "claim": claim,
        "why": why,
        "scenario": scenario,
        "suggested_fix": suggested_fix,
        "confidence": 0.7 if ev["asked"] else 0.9,
        "reviewers": ["compiled"],
        "source": f"compiled:{step['id']}",
        "citations": citations,
        "beyond_diff": beyond,
        "evidence": _clip(" | ".join(ev["parts"]), EVIDENCE_CHARS),
        "program_step": step["id"],
    }


class Interpreter:
    """Runs one validated program against the repository at the reviewed commit."""

    def __init__(
        self,
        program: Program,
        repo: deep.Repo,
        *,
        frames: Any,
        session: Any,
        diff_lines: dict[str, set[int]],
        ask_model: str | None,
        ask_thinking: str | None,
        cutoff: float | None,
        clock: Callable[[], float],
        cap: CapFn,
        to_level: LevelFn | None = None,
        enrich: Callable[[Any, dict[str, Any]], None] | None = None,
        generic: Callable[[dict[str, Any]], str | None] | None = None,
        unavailable: set[tuple[str, str]] | None = None,
        max_steps: int = MAX_STEPS,
        defer_unrunnable: bool = False,
    ) -> None:
        self.program = program
        self.max_steps = max_steps
        self.defer_unrunnable = defer_unrunnable
        self.max_planned = LARGE_PROGRAM_STEPS if max_steps > MAX_STEPS else MAX_PROGRAM_STEPS
        self.repo = repo
        self.frames = frames
        self.session = session
        self.diff_lines = diff_lines

        self.ask_model = ask_model
        self.ask_thinking = ask_thinking
        self.cutoff = cutoff
        self.clock = clock
        self.cap = cap
        self.to_level = to_level
        self.enrich = enrich
        self.generic = generic
        self.unavailable: set[tuple[str, str]] = set(unavailable) if unavailable else set()

        self.results: dict[str, dict[str, Any]] = {}
        self.records: list[dict[str, Any]] = []
        self.findings: list[dict[str, Any]] = []
        self.dropped: list[str] = []
        self.generic_dropped: list[str] = []
        self.refuted = 0
        self.truncated: list[str] = []
        self.steps_by_id: dict[str, dict[str, Any]] = {}

        self.expanded = 0
        self.asks = 0
        self.auto_asks = 0
        self.resolved = 0
        self.not_emitted = {
            "gateFalse": 0,
            "undecided": 0,
            "askedNo": 0,
            "askedUnclear": 0,
        }
        self.test_runs_before = len(session.records) if session is not None else 0

    def _value(self, ref: str, scope: dict[str, Any]) -> str | None:
        name, _, field = ref.partition(".")
        if name in scope:
            value = scope[name]
            if not field:
                return _render(value)
            return _render(value.get(field)) if isinstance(value, dict) else None

        result = self.results.get(name)
        if result is None:
            return None
        if not field:
            return result.get("summary")
        if field == "text":
            return result.get("text", "")
        if field not in _RESULT_SCALAR_FIELDS:
            return None
        if field == "status" and result["op"] in TEST_OPS:
            return _render(result.get("outcome"))
        return _render(result.get(field))

    def fill(self, text: Any, scope: dict[str, Any] | None = None) -> Any:
        """Replace every {{placeholder}} with the named result or item field."""
        if isinstance(text, dict):
            return {key: self.fill(value, scope) for key, value in text.items()}
        if isinstance(text, list):
            return [self.fill(value, scope) for value in text]
        if not isinstance(text, str):
            return text

        active_scope = as_dict(scope)

        def replace(match: re.Match[str]) -> str:
            value = self._value(match.group(1), active_scope)
            return match.group(0) if value is None else value

        return _PLACEHOLDER.sub(replace, text)

    def truth(self, sid: str) -> bool | None:
        """Whether an assert or ask step holds: True, False, or None when unknown."""
        result = self.results.get(sid)
        if result is None or result["status"] != "ok":
            return None
        if result["op"] == "assert":
            return result.get("value")
        if result["op"] == "ask":
            answer = result.get("answer")
            return None if answer == "unclear" else answer == "yes"
        return None

    def _condition(self, when: dict[str, Any] | None) -> bool | None:
        if when is None:
            return True
        value = self.truth(when["step"])
        if value is None:
            return None
        return not value if when["not"] else value

    def roots_in_ask(self, sid: str, seen: set[str] | None = None) -> bool:
        """Whether a step's truth depends, transitively, on a model's answer."""
        visited = seen if seen is not None else set()
        if sid in visited:
            return False
        visited.add(sid)
        step = self.steps_by_id.get(sid)
        if step is None:
            return False
        if step["op"] == "ask":
            return True
        if step["op"] != "assert":
            return False
        refs = (
            ([step["step"]] if isinstance(step.get("step"), str) else [])
            + as_list(step.get("all"))
            + as_list(step.get("any"))
        )
        return any(self.roots_in_ask(ref, visited) for ref in refs)

    def contradicted(self, sid: str, seen: set[str] | None = None) -> bool:
        """Whether a deterministic assert contradicted the expectation."""
        visited = seen if seen is not None else set()
        if sid in visited:
            return False
        visited.add(sid)
        step = self.steps_by_id.get(sid)
        result = self.results.get(sid)
        if step is None or step["op"] != "assert" or result is None:
            return False
        if result.get("contradicted"):
            return True
        refs = as_list(step.get("all")) + as_list(step.get("any"))
        return any(self.contradicted(ref, visited) for ref in refs)

    def test_decided(self, sid: str) -> bool:
        """Whether an assert reads a test step whose run finished."""
        step = as_dict(self.steps_by_id.get(sid))
        if step.get("op") != "assert" or not isinstance(step.get("step"), str):
            return False
        target = as_dict(self.results.get(step["step"]))
        return target.get("op") in TEST_OPS and target.get("outcome") in ("passed", "failed")

    def test_could_not_run(self, sid: str) -> bool:
        """Whether a gate rests on a test step that could not run."""
        leaves = _assert_leaves(sid, self.steps_by_id)
        return any(
            leaf["op"] in TEST_OPS
            and as_dict(self.results.get(leaf["id"])).get("outcome") == "could_not_run"
            for leaf in leaves
        )

    def _summary(self, op: str, result: dict[str, Any]) -> str:
        if op in TEST_OPS:
            return result.get("title", "") + ": " + result.get("outcome", "")
        if op == "ask":
            quote = _clip(result.get("quote") or "", 160)
            return f"answer {result.get('answer')} (quote: {quote})"
        if op == "assert":
            return _summarize_assert(result)
        if op == "finding":
            return _summarize_finding(result)
        return _summarize_lookup(op, result)

    def _ask_tokens(self, step: dict[str, Any], extra: dict[str, Any]) -> int:
        if step["op"] != "ask" and not extra.get("resolved"):
            return 0
        label = extra.get("ask") or step["id"]
        return sum(
            int(item.get("tokens") or 0)
            for item in self.frames.timings
            if item.get("phase") == "ask" and item.get("reviewer") == label
        )

    def _record(
        self,
        step: dict[str, Any],
        result: dict[str, Any],
        began: float,
        inputs: Any,
        **extra: Any,
    ) -> None:
        result["summary"] = self._summary(step["op"], result)
        output_text = result.get("text") if result.get("text") else result["summary"]
        entry: dict[str, Any] = {
            "id": step["id"],
            "op": step["op"],
            "status": result["status"],
            "ms": int((self.clock() - began) * 1000),
            "tokens": self._ask_tokens(step, extra),
            "input": _clip(json.dumps(inputs, default=str), RECORD_CHARS),
            "output": _clip(output_text, RECORD_CHARS),
        }
        if result.get("detail"):
            entry["detail"] = result["detail"]
        for key, value in extra.items():
            if value is not None:
                entry[key] = value
        self.records.append(entry)
        self.results[step["id"]] = result

    def _skip(self, step: dict[str, Any], why: str, began: float) -> None:
        self._record(
            step,
            {"op": step["op"], "status": "skipped", "detail": why, "text": ""},
            began,
            None,
        )

    def _unavailable_runner(self, args: dict[str, Any]) -> str | None:
        """Why a test step will not run before trying."""
        if self.session is None:
            return None
        paths = args.get("paths") if "paths" in args else args.get("tests")
        try:
            groups = self.session.plan(self.session.paths(paths))
        except testing.TestsRejected:
            return None
        for runner, directory, _relative in groups:
            if (runner.name, directory or ".") in self.unavailable:
                return (
                    f"the {runner.name} runner in {directory or '.'} was unavailable in an "
                    "earlier run (missing dependencies); not tried again"
                )
        return None

    def _attach_test_outcome(self, title: str, result: dict[str, Any]) -> None:
        record = _find_test_record(self.session, title)
        result["run"] = record
        status = as_dict(record).get("status")
        result["outcome"] = status if status in ("passed", "failed") else "could_not_run"
        if record is None:
            return
        if record.get("mutation"):
            result["caught"] = bool(record.get("caught"))
        if status == "unavailable":
            self.unavailable.add((record["runner"], record.get("cwd") or "."))

    def _serve_step_op(self, op_name: str, args: Any) -> tuple[str, str]:
        if op_name in TEST_OPS:
            if self.session is None:
                raise deep.Rejected("tests are not run in this review")
            why = self._unavailable_runner(as_dict(args))
            if why:
                raise deep.Rejected(why)
        return deep.serve_request(self.repo, {op_name: args}, self.session)

    def _lookup(self, step: dict[str, Any], scope: dict[str, Any]) -> None:
        began = self.clock()
        args = self.fill(step["args"], scope)
        op_name = step["op"]
        try:
            title, body = self._serve_step_op(op_name, args)
        except deep.Rejected as error:
            outcome: dict[str, Any] = {
                "op": op_name,
                "status": "failed",
                "detail": str(error),
                "text": "",
            }
            if op_name in TEST_OPS:
                outcome.update(status="ok", outcome="could_not_run")
            self._record(step, outcome, began, args)
            return

        result: dict[str, Any] = {
            "op": op_name,
            "status": "ok",
            "title": title,
            "text": _clip(body, RESULT_CHARS),
            "count": _count_of(op_name, title, body),
            "items": _items_of(op_name, body),
            "capped": CAPPED in title,
            "truncated": len(body) > RESULT_CHARS or "... cut" in body,
        }
        if op_name in TEST_OPS:
            self._attach_test_outcome(title, result)
        self._record(step, result, began, args)

    def _eval_contains_predicate(
        self,
        op: str,
        negated: str,
        target: dict[str, Any],
    ) -> tuple[bool | None, str]:
        needle = self.fill(op)
        present = _flat(needle) in _flat(target.get("text") or "")
        if not present and target.get("truncated"):
            return None, f"{needle!r} is not in the result, which is truncated"
        value = not present if negated else present
        verb = "is" if present else "is not"
        return value, f"{needle!r} {verb} in the result"

    def _eval_single_assert(self, step: dict[str, Any]) -> tuple[bool | None, str]:
        target_id = step["step"]
        target = self.results.get(target_id)
        kind, op, operand = step["predicate_parsed"]
        if target is None or target["status"] != "ok":
            return None, f"{target_id} did not run"

        unrunnable_test = (
            target["op"] in TEST_OPS
            and target.get("outcome") == "could_not_run"
            and not (kind == "status" and operand == "could_not_run")
        )
        if unrunnable_test:
            return None, f"{target_id} could not run"

        if kind == "count":
            count = target.get("count")
            if count is None:
                return None, f"{target_id} has no count"
            return _count_predicate(count, bool(target.get("capped")), op, int(operand))

        if kind in ("status", "answer"):
            return _eval_word_predicate(kind, op, operand, target)
        return self._eval_contains_predicate(op, operand, target)

    def _eval_compound_assert(self, step: dict[str, Any]) -> tuple[bool | None, str]:
        refs = as_list(step.get("all")) or as_list(step.get("any"))
        values = [self.truth(ref) for ref in refs]
        value = _eval_compound_truth(bool(step.get("all")), values)
        detail = ", ".join(
            f"{ref}={'?' if item is None else item}" for ref, item in zip(refs, values)
        )
        return value, detail

    def _assert(self, step: dict[str, Any]) -> None:
        began = self.clock()
        if isinstance(step.get("step"), str):
            value, detail = self._eval_single_assert(step)
        else:
            value, detail = self._eval_compound_assert(step)

        expect = step.get("expect")
        contradicted = (
            expect is not None
            and value is not None
            and value != expect
            and not self.roots_in_ask(step["id"])
        )
        holds_text = self.fill(step.get("holds") or "") if value and not contradicted else ""
        inputs = step.get("predicate") or {"all": step.get("all"), "any": step.get("any")}
        payload = {
            "op": "assert",
            "status": "ok",
            "value": value,
            "detail": detail,
            "holds": holds_text,
            "text": "",
            "expected": expect,
            "contradicted": contradicted,
        }
        self._record(step, payload, began, inputs)

    def _material(self, refs: list[str], left: int) -> tuple[list[str], list[str]]:
        """Result views for the given steps, as labelled data, within `left` chars."""
        views: list[str] = []
        texts: list[str] = []
        for ref in refs:
            result = self.results.get(ref)
            if result is None or result["status"] != "ok":
                continue
            text = result.get("text") or result.get("summary") or ""
            if len(text) > left:
                text = text[: max(0, left - 1)] + "…"
            left -= len(text)
            title = result.get("title", result["op"])
            views.append(
                f"Material from step {ref} ({title}; untrusted repository data "
                f"read by the host at the reviewed commit):\n{text}"
            )
            texts.append(text)
        return views, texts

    def _ask_views(
        self,
        step: dict[str, Any],
        scope: dict[str, Any],
    ) -> tuple[list[str], str] | None:
        """The views of an ask and the material the quote must come from."""
        for ref in step["context"]:
            result = self.results.get(ref)
            if result is None or result["status"] != "ok":
                return None

        views, texts = self._material(step["context"], ASK_CONTEXT_CHARS)
        for item in as_list(step.get("material")):
            views.append(
                "Material attached to the question (untrusted repository data read by the "
                f"host at the reviewed commit):\n{item}"
            )
            texts.append(item)
        return [f"Question: {self.fill(step['question'], scope)}", *views], "\n".join(texts)

    def _quote_matches_material(self, quote: str, material: str) -> bool:
        if len(quote) < 4:
            return False
        if quote in _flat(material):
            return True
        for line in material.splitlines():
            flat_line = _flat(line)
            if len(flat_line) >= 8 and flat_line in quote:
                return True
        return False

    def _check_quote(self, reply: Any, material: str) -> tuple[str, str, bool, str]:
        """Return (answer, quote, quote_ok, detail) checked against material."""
        if not isinstance(reply, dict):
            return "unclear", "", False, "no reply"

        answer = str(reply.get("answer", "")).strip().lower()
        raw_quote = reply.get("quote")
        quote = _flat(raw_quote) if isinstance(raw_quote, str) else ""
        quote_ok = self._quote_matches_material(quote, material)

        if answer not in ANSWERS:
            return "unclear", quote, quote_ok, f"the answer {answer!r} is not yes, no or unclear"
        if answer != "unclear" and not quote_ok:
            return (
                "unclear",
                quote,
                quote_ok,
                "the quote is not in the material: the answer counts as unclear",
            )
        return answer, quote, quote_ok, ""

    def _record_ask_reply(
        self,
        step: dict[str, Any],
        views: list[str],
        material: str,
        began: float,
        reply: Any,
    ) -> None:
        if isinstance(reply, (Incomplete, FrameError)) or not isinstance(reply, dict):
            why = _error_reason(reply)
            failed = {"op": "ask", "status": "failed", "detail": _text(why, 160), "text": ""}
            self._record(step, failed, began, views[0])
            return

        answer, quote, quote_ok, detail = self._check_quote(reply, material)
        why_text = _text(reply.get("why"), 300)
        payload = {
            "op": "ask",
            "status": "ok",
            "answer": answer,
            "quote": quote,
            "quote_ok": quote_ok,
            "why": why_text,
            "detail": detail,
            "text": f"answer: {answer}\nquote: {quote}\nwhy: {why_text}",
        }
        self._record(step, payload, began, views[0])

    async def _asks(self, steps: list[tuple[dict[str, Any], dict[str, Any]]]) -> None:
        jobs = []
        prepared = []
        for step, scope in steps:
            began = self.clock()
            if self.asks >= MAX_ASKS:
                self._skip(step, f"the limit of {MAX_ASKS} asks is reached", began)
                continue
            views = self._ask_views(step, scope)
            if views is None:
                self._skip(step, "a context step did not run", began)
                continue
            self.asks += 1
            jobs.append((step["id"], ASK_TASK, views[0]))
            prepared.append((step, views[0], views[1], began))

        if not jobs:
            return

        replies = await self.frames.run(
            "ask",
            jobs,
            contract=ASK_CONTRACT,
            model=self.ask_model,
            thinking=self.ask_thinking,
            cutoff=self.cutoff,
        )
        for (step, views, material, began), reply in zip(prepared, replies):
            self._record_ask_reply(step, views, material, began, reply)

    def _expand(self, step: dict[str, Any]) -> list[dict[str, Any]]:
        """The instances of a for_each template over the items of its `over` result."""
        source = self.results.get(step["over"])
        ok_source = source is not None and source["status"] == "ok"
        items = list(as_list(source.get("items"))) if ok_source else []

        instances: list[dict[str, Any]] = []
        template_ids = {sub["id"] for sub in step["steps"]}
        total = len(self.steps_by_id)

        for index, item in enumerate(items[: step["max_items"]]):
            if total + len(instances) + len(step["steps"]) > self.max_steps:
                self.truncated.append(
                    f"{step['id']}: items from {index} on were not expanded "
                    f"({self.max_steps} steps at most)"
                )
                break
            scope = {"item": item, "index": index}
            rename = {sid: f"{step['id']}[{index}].{sid}" for sid in template_ids}
            for sub in step["steps"]:
                instances.append(_clone_template_step(sub, rename, scope, step["id"]))

        if len(items) > step["max_items"]:
            excess = len(items) - step["max_items"]
            self.truncated.append(
                f"{step['id']}: {excess} items beyond max_items were not visited"
            )
        self.expanded += len(instances)
        return instances

    def _collect_check_summaries(self, when_step_id: str) -> tuple[list[str], list[str]]:
        gate = as_dict(self.steps_by_id.get(when_step_id))
        candidate_ids = [when_step_id, *as_list(gate.get("all")), *as_list(gate.get("any"))]
        checks: list[str] = []
        leaves: list[str] = []
        for sid in candidate_ids:
            check = self.steps_by_id.get(sid)
            if check is None or check["op"] != "assert":
                continue
            result = as_dict(self.results.get(sid))
            checks.append(_format_single_check_line(sid, check, result))
            if isinstance(check.get("step"), str):
                leaves.append(check["step"])
        return checks, leaves

    def _append_source_window(
        self,
        public: dict[str, Any],
        views: list[str],
        texts: list[str],
    ) -> None:
        try:
            path = self.repo.path(public["file"])
            line = int(public["line"])
            lines = self.repo.lines(path)
        except (deep.Rejected, TypeError, ValueError):
            return
        if not lines or not 1 <= line <= len(lines):
            return
        window = source_window(lines, line, 12)
        views.append(
            f"Source of {path} around line {line} (> marks the line the finding names):\n"
            f"{window}"
        )
        texts.append(window)

    def _append_closest_test_window(
        self,
        step: dict[str, Any],
        views: list[str],
        texts: list[str],
    ) -> None:
        unpinned = step.get("unpinned")
        if not isinstance(unpinned, dict):
            return
        closest = unpinned.get("closest_test")
        if not isinstance(closest, dict) or not isinstance(closest.get("path"), str):
            return
        try:
            test_path = self.repo.path(closest["path"])
            test_lines = self.repo.lines(test_path)
            line_num = int(closest.get("line") or 1)
        except (deep.Rejected, TypeError, ValueError):
            return
        if not test_lines:
            return
        at = min(max(1, line_num), len(test_lines))
        window = source_window(test_lines, at, 25)
        views.append(
            f"The existing test nearest to it ({test_path}, around line {at}):\n{window}"
        )
        texts.append(window)

    def _resolve_views(
        self,
        step: dict[str, Any],
        scope: dict[str, Any],
        why: str,
    ) -> tuple[list[str], str]:
        """The question and raw material for an undecided finding."""
        when = step["when"]
        public = {
            "file": self.fill(step["file"], scope),
            "line": self.fill(step["line"], scope),
            "claim": self.fill(step["claim"], scope),
            "why": self.fill(step.get("why") or "", scope),
            "scenario": self.fill(step.get("scenario") or "", scope),
        }
        checks, leaves = self._collect_check_summaries(when["step"])
        negated = (
            " The finding is planned for when the check does NOT hold." if when["not"] else ""
        )
        views = [
            f"Finding (as the planner wrote it; its check came out {why}):\n"
            f"{json.dumps(public, indent=1)}",
            "The check:\n" + "\n".join(checks) + negated,
        ]

        refs = list(dict.fromkeys(leaves + list(as_list(step.get("evidence")))))
        material_views, texts = self._material(refs, ASK_CONTEXT_CHARS)
        views.extend(material_views)
        self._append_source_window(public, views, texts)
        self._append_closest_test_window(step, views, texts)
        return views, "\n".join(texts)

    def _record_non_emitted(
        self,
        step: dict[str, Any],
        began: float,
        counter: str,
        gate: str,
        detail: str,
    ) -> None:
        self.not_emitted[counter] += 1
        self._record(step, _record_non_emitted_payload(gate, detail), began, None)

    def _check_assert_finding_gate(
        self,
        step: dict[str, Any],
        scope: dict[str, Any],
        when_id: str,
        held: bool | None,
        began: float,
    ) -> tuple[bool, tuple[dict[str, Any], dict[str, Any], str] | None]:
        """Handle assert-gated findings; return (handled, deferred_item)."""
        if self.contradicted(when_id) and not self.test_decided(when_id):
            return True, (step, scope, "against the planner's expectation")
        if held is not None:
            return False, None
        if self.defer_unrunnable and self.test_could_not_run(when_id):
            detail = (
                f"its check {when_id} rests on a test that could not run "
                "(left to the verifier)"
            )
            self._record_non_emitted(
                step, began, "undecided", "gate unknown; the test could not run", detail
            )
            return True, None
        return True, (step, scope, "unknown")

    def _finding(
        self,
        step: dict[str, Any],
        scope: dict[str, Any],
    ) -> tuple[dict[str, Any], dict[str, Any], str] | None:
        """Run a finding step; return (step, scope, why) when the host must ask."""
        began = self.clock()
        when = step["when"]
        held = self._condition(when)
        gate = self.steps_by_id.get(when["step"])

        if gate is not None and gate["op"] == "assert":
            handled, deferred = self._check_assert_finding_gate(
                step, scope, when["step"], held, began
            )
            if handled:
                return deferred

        if held is None:
            detail = f"its condition {when['step']} did not decide (no finding)"
            self._record_non_emitted(step, began, "undecided", "gate undecided", detail)
            return None

        if not held:
            detail = f"{when['step']} does not hold (no finding)"
            self._record_non_emitted(step, began, "gateFalse", "gate false", detail)
            return None

        self._emit(step, scope, began, None)
        return None

    def _emit(
        self,
        step: dict[str, Any],
        scope: dict[str, Any],
        began: float,
        resolution: dict[str, Any] | None,
        **extra: Any,
    ) -> None:
        finding, problem = self._compose(step, scope, resolution)
        gate = "gate unknown -> ask: yes" if resolution is not None else "gate true"
        if finding is None:
            self.dropped.append(f"{step['id']}: {problem}")
            payload = _record_non_emitted_payload(
                f"{gate}; dropped: {problem}", f"dropped: {problem}"
            )
            self._record(step, payload, began, None, **extra)
            return

        reason = self.generic(finding) if self.generic is not None else None
        if reason:
            self.generic_dropped.append(reason)
            payload = _record_non_emitted_payload(
                f"{gate}; dropped: {reason}", f"dropped: {reason}"
            )
            self._record(step, payload, began, None, **extra)
            return

        if finding.get("proof") == "refuted":
            self.refuted += 1
            payload = _record_non_emitted_payload(
                f"{gate}; refuted by a test",
                "refuted: the mutant it names was caught by a test",
            )
            self._record(step, payload, began, None, **extra)
            return

        self.findings.append(finding)
        summary_payload = {key: finding[key] for key in ("file", "line", "level", "claim")}
        detail = (
            finding.get("how_verified")
            or "rests on a small-model answer: goes to the verifier"
        )
        emitted_payload = {
            "op": "finding",
            "status": "ok",
            "emitted": True,
            "text": "finding emitted: " + json.dumps(summary_payload),
            "detail": detail,
        }
        self._record(step, emitted_payload, began, None, **extra)

    def _prepare_resolve_jobs(
        self,
        deferred: list[tuple[dict[str, Any], dict[str, Any], str]],
    ) -> tuple[list[tuple[str, str, list[str]]], list[tuple[Any, ...]]]:
        jobs: list[tuple[str, str, list[str]]] = []
        prepared: list[tuple[Any, ...]] = []
        for step, scope, why in deferred:
            began = self.clock()
            if self.asks >= MAX_ASKS:
                self.not_emitted["undecided"] += 1
                detail = f"its check came out {why}; the limit of {MAX_ASKS} asks is reached"
                payload = _record_non_emitted_payload("gate unknown -> no ask left", detail)
                self._record(step, payload, began, None, resolved="ask")
                continue
            views, material = self._resolve_views(step, scope, why)
            self.asks += 1
            self.auto_asks += 1
            jobs.append((f"{step['id']}.ask", RESOLVE_TASK, views))
            prepared.append((step, scope, why, views, material, began))
        return jobs, prepared

    def _apply_resolve_reply(self, item: tuple[Any, ...], reply: Any) -> None:
        step, scope, why, views, material, began = item
        label = f"{step['id']}.ask"
        if isinstance(reply, (Incomplete, FrameError)) or not isinstance(reply, dict):
            error = _error_reason(reply)
            failed = {
                "op": "finding",
                "status": "failed",
                "text": "",
                "detail": (
                    f"its check came out {why}; the question to the small model "
                    f"failed ({_text(error, 160)})"
                ),
            }
            self._record(step, failed, began, views[0], resolved="ask", ask=label)
            return

        answer, quote, _quote_ok, detail = self._check_quote(reply, material)
        self.resolved += 1
        resolution = {
            "id": label,
            "answer": answer,
            "quote": quote,
            "why": _text(reply.get("why"), 300),
            "question": f"does the finding hold although its check came out {why}?",
        }
        if answer == "yes":
            self._emit(step, scope, began, resolution, resolved="ask", ask=label, answer=answer)
            return

        self.not_emitted["askedNo" if answer == "no" else "askedUnclear"] += 1
        payload = _record_non_emitted_payload(
            f"gate unknown -> ask: {answer}",
            _format_resolve_detail(why, answer, detail, quote),
        )
        self._record(step, payload, began, views[0], resolved="ask", ask=label, answer=answer)

    async def _resolve(
        self,
        deferred: list[tuple[dict[str, Any], dict[str, Any], str]],
    ) -> None:
        """Ask the small model about findings whose check is unknown or contradicted."""
        jobs, prepared = self._prepare_resolve_jobs(deferred)
        if not jobs:
            return
        replies = await self.frames.run(
            "ask",
            jobs,
            contract=ASK_CONTRACT,
            model=self.ask_model,
            thinking=self.ask_thinking,
            cutoff=self.cutoff,
        )
        for item, reply in zip(prepared, replies):
            self._apply_resolve_reply(item, reply)

    def _append_ask_evidence(
        self,
        ref: str,
        result: dict[str, Any],
        scope: dict[str, Any],
        state: dict[str, Any],
    ) -> None:
        state["asked"] = True
        quote = result.get("quote") or ""
        question = self.fill(self.steps_by_id[ref]["question"], scope)
        quote_suffix = f", quoting `{quote}`" if quote else ""
        state["ask_notes"].append(f"{ref}: {question} -> {result['answer']}{quote_suffix}")
        short_quote = f" quoting `{_clip(quote, 160)}`" if quote else ""
        state["parts"].append(f"[{ref}] small model answered {result['answer']}{short_quote}")

    def _append_evidence_ref(
        self,
        ref: str,
        scope: dict[str, Any],
        state: dict[str, Any],
    ) -> None:
        result = self.results.get(ref)
        if result is None or result["status"] != "ok":
            state["parts"].append(f"[{ref}] did not run")
            return

        state["usable"] += 1
        if result["op"] in TEST_OPS and result.get("run"):
            run = result["run"]
            if state["test_run"] is None:
                state["test_run"] = run
            if result["outcome"] in ("passed", "failed"):
                state["observed"] = True
            summary = testing.summarize(run).split("\n  output")[0].replace("\n", " ")
            state["parts"].append(f"[{ref}] {summary}")
        elif result["op"] == "ask":
            self._append_ask_evidence(ref, result, scope, state)
        else:
            state["parts"].append(f"[{ref}] {result['summary']}")

    def _collect_evidence(
        self,
        step: dict[str, Any],
        scope: dict[str, Any],
        resolution: dict[str, Any] | None,
    ) -> dict[str, Any]:
        state: dict[str, Any] = {
            "parts": [],
            "ask_notes": [],
            "test_run": None,
            "observed": False,
            "asked": resolution is not None or self.roots_in_ask(step["when"]["step"]),
            "usable": 0,
        }
        for ref in step["evidence"]:
            self._append_evidence_ref(ref, scope, state)

        if resolution is not None:
            note, part = _format_resolution_note(resolution)
            state["ask_notes"].append(note)
            state["parts"].append(part)
            state["usable"] += 1
        return state

    def _compute_level(
        self,
        step: dict[str, Any],
        category: str,
        scenario: str,
        asked: bool,
        observed: bool,
    ) -> tuple[str, str]:
        name = str(step.get("level", "")).strip().lower()
        if self.to_level is not None:
            stated = self.to_level(name) or "low"
        elif name in LEVELS:
            stated = name
        else:
            stated = map_legacy_level(name) or "low"

        if asked:
            return stated, self.cap(stated, category, scenario)

        level = self.cap(stated, category, scenario, True if observed else None)
        if level in ("critical", "high") and not observed:
            level = "medium"
        return stated, level

    def _find_mutation_result(self, step: dict[str, Any]) -> dict[str, Any] | None:
        for ref in step["evidence"]:
            result = self.results.get(ref)
            if result is None or result["op"] != "mutation_check":
                continue
            if result["status"] == "ok" and result.get("run"):
                return result
        return None

    def _attach_mutation_proof(
        self,
        step: dict[str, Any],
        category: str,
        finding: dict[str, Any],
    ) -> None:
        if category != "tests":
            return
        mutation = self._find_mutation_result(step)
        if mutation is None:
            return
        if mutation.get("caught"):
            finding["proof"] = "refuted"
        elif mutation["outcome"] == "passed":
            finding["proof"] = "proven"
            finding.setdefault("unpinned", {"behaviour": "", "change": ""})

    def _resolve_finding_location(
        self,
        step: dict[str, Any],
        scope: dict[str, Any],
    ) -> tuple[str | None, int | None, str | None]:
        try:
            path = self.repo.path(self.fill(step["file"], scope))
            line = int(self.fill(step["line"], scope))
        except (deep.Rejected, TypeError, ValueError) as error:
            return None, None, f"no tracked file and line ({error})"
        lines = self.repo.lines(path)
        if not lines or not 1 <= line <= len(lines):
            return None, None, f"{path} has no line {line}"
        return path, line, None

    def _filled_text(
        self,
        step: dict[str, Any],
        key: str,
        scope: dict[str, Any],
        limit: int,
    ) -> str:
        raw = step.get(key)
        return _text(self.fill(raw if raw else "", scope), limit)

    def _compose(
        self,
        step: dict[str, Any],
        scope: dict[str, Any],
        resolution: dict[str, Any] | None = None,
    ) -> tuple[dict[str, Any] | None, str | None]:
        """Compose a finding with its evidence: (finding, None) or (None, drop_reason)."""
        path, line, loc_error = self._resolve_finding_location(step, scope)
        if path is None or line is None:
            return None, loc_error

        citations, problem, beyond = deep.check_citations(
            self.repo,
            self.fill(step["citations"], scope),
            self.diff_lines,
        )
        if step["citations"] and problem:
            return None, problem

        ev = self._collect_evidence(step, scope, resolution)
        if ev["usable"] == 0 and not citations:
            return None, "no evidence: none of the steps it names produced a result"

        is_beyond = beyond or line not in self.diff_lines.get(path, set())
        if ev["test_run"] is not None:
            is_beyond = True
        category = normalize_category(_text(step.get("category"), 40), "bugs")
        scenario = self._filled_text(step, "scenario", scope, 500)
        stated, level = self._compute_level(step, category, scenario, ev["asked"], ev["observed"])

        finding = _build_finding_dict(
            step,
            path=path,
            line=line,
            level=level,
            stated=stated,
            category=category,
            claim=self._filled_text(step, "claim", scope, 300),
            why=self._filled_text(step, "why", scope, 600),
            scenario=scenario,
            suggested_fix=self._filled_text(step, "fix", scope, 500),
            citations=citations,
            beyond=is_beyond,
            ev=ev,
        )
        self._finalize_finding(step, scope, finding, citations, ev)
        return finding, None

    def _finalize_finding(
        self,
        step: dict[str, Any],
        scope: dict[str, Any],
        finding: dict[str, Any],
        citations: list[dict[str, Any]],
        ev: dict[str, Any],
    ) -> None:
        if ev["test_run"] is not None:
            finding["test_run"] = ev["test_run"]["n"]
            finding["test_evidence"] = testing.summarize(ev["test_run"]).split("\n  output")[0]
        if self.enrich is not None:
            payload = self.fill(
                {"unpinned": step.get("unpinned"), "consequence": step.get("consequence")},
                scope,
            )
            self.enrich(payload, finding)
        self._attach_mutation_proof(step, finding["category"], finding)
        if ev["asked"]:
            finding["ask_evidence"] = "\n".join(ev["ask_notes"])
            return
        cited = "; ".join(f"{item['path']}:{item['line']} `{item['quote']}`" for item in citations)
        finding["host_confirmed"] = True
        finding["how_verified"] = (
            "the review program's evidence, produced by the host: "
            + _clip(" | ".join(ev["parts"]) or cited, 300)
        )

    def _check_step_gate(self, step: dict[str, Any], began: float) -> bool:
        if step["op"] == "finding":
            return True
        held = self._condition(step.get("when"))
        if held is None:
            self._skip(step, f"its condition {step['when']['step']} did not decide", began)
            return False
        if held is False:
            self._skip(step, f"its condition {step['when']['step']} does not hold", began)
            return False
        return True

    async def execute(self, step: dict[str, Any]) -> dict[str, Any]:
        """Run one step whose dependencies have all finished, and return its result."""
        self.steps_by_id[step["id"]] = step
        scope: dict[str, Any] = {}
        began = self.clock()
        if self._check_step_gate(step, began):
            await self._execute_gated_step(step, scope)
        return self.results.get(step["id"]) or {"op": step["op"], "status": "skipped"}

    async def _execute_gated_step(self, step: dict[str, Any], scope: dict[str, Any]) -> None:
        op_name = step["op"]
        if op_name in LOOKUPS or op_name in TEST_OPS:
            self._lookup(step, scope)
        elif op_name == "assert":
            self._assert(step)
        elif op_name == "ask":
            await self._asks([(step, scope)])
        elif op_name == "finding":
            undecided = self._finding(step, scope)
            if undecided is not None:
                await self._resolve([undecided])
        elif op_name == "for_each":
            await self.run([step])

    def _expand_and_register(self, step: dict[str, Any]) -> list[dict[str, Any]]:
        instances = self._expand(step)
        step["instances"] = [item["id"] for item in instances]
        for item in instances:
            self.steps_by_id[item["id"]] = item
        return instances

    def _record_for_each_summary(self, step: dict[str, Any], began: float) -> None:
        done = [self.results[sid] for sid in step["instances"]]
        emitted = sum(1 for item in done if item.get("emitted"))
        held_count = sum(1 for item in done if item["op"] == "assert" and item.get("value"))
        payload = {
            "op": "for_each",
            "status": "ok",
            "count": len(step["instances"]),
            "text": "",
            "items": [],
            "detail": f"{emitted} finding(s) emitted, {held_count} assert(s) held",
        }
        self._record(step, payload, began, {"over": step["over"], "instances": len(step["instances"])})

    def _run_for_each_step(self, step: dict[str, Any], began: float) -> list[dict[str, Any]]:
        if "instances" not in step:
            instances = self._expand_and_register(step)
            if instances:
                return instances
        self._record_for_each_summary(step, began)
        return []

    def _dispatch_ready_step(
        self,
        step: dict[str, Any],
        asks: list[tuple[dict[str, Any], dict[str, Any]]],
        deferred: list[tuple[dict[str, Any], dict[str, Any], str]],
        added: list[dict[str, Any]],
    ) -> None:
        began = self.clock()
        if not self._check_step_gate(step, began):
            return

        scope = as_dict(step.get("scope"))
        op_name = step["op"]
        if op_name == "for_each":
            added.extend(self._run_for_each_step(step, began))
        elif op_name in LOOKUPS or op_name in TEST_OPS:
            self._lookup(step, scope)
        elif op_name == "assert":
            self._assert(step)
        elif op_name == "finding":
            undecided = self._finding(step, scope)
            if undecided is not None:
                deferred.append(undecided)
        elif op_name == "ask":
            asks.append((step, scope))

    def _is_step_ready(self, step: dict[str, Any]) -> bool:
        refs_done = all(ref in self.results for ref in _references(step))
        instances_done = all(ref in self.results for ref in step.get("instances", []))
        return refs_done and instances_done

    async def _run_ready_wave(
        self,
        ready: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        asks: list[tuple[dict[str, Any], dict[str, Any]]] = []
        deferred: list[tuple[dict[str, Any], dict[str, Any], str]] = []
        added: list[dict[str, Any]] = []
        for step in ready:
            self._dispatch_ready_step(step, asks, deferred, added)

        if asks:
            await self._asks(asks)
        if deferred:
            await self._resolve(deferred)
        return added

    def _init_pending(self, steps: list[dict[str, Any]] | None) -> list[dict[str, Any]]:
        source_steps = self.program.steps if steps is None else steps
        pending = [dict(step) for step in source_steps]
        for step in pending:
            self.steps_by_id[step["id"]] = step
        return pending

    def _skip_unfinished(self, pending: list[dict[str, Any]]) -> None:
        for step in pending:
            self._skip(step, "its dependencies never finished", self.clock())

    def _advance_pending(
        self,
        pending: list[dict[str, Any]],
        ready: list[dict[str, Any]],
        added: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        finished = {step["id"] for step in ready if step["id"] in self.results}
        return [step for step in pending if step["id"] not in finished] + added

    async def run(self, steps: list[dict[str, Any]] | None = None) -> None:
        pending = self._init_pending(steps)
        while pending:
            ready = [step for step in pending if self._is_step_ready(step)]
            if not ready:
                self._skip_unfinished(pending)
                break
            added = await self._run_ready_wave(ready)
            pending = self._advance_pending(pending, ready, added)

    def _ok_assert_results(self) -> list[dict[str, Any]]:
        return [
            self.results[record["id"]]
            for record in self.records
            if record["op"] == "assert" and record["status"] == "ok"
        ]

    def _test_run_count(self) -> int:
        if self.session is None:
            return 0
        return len(self.session.records) - self.test_runs_before

    def stats(self) -> dict[str, Any]:
        by_status: dict[str, int] = {}
        for record in self.records:
            by_status[record["status"]] = by_status.get(record["status"], 0) + 1

        deterministic = sum(1 for item in self.findings if item.get("host_confirmed"))
        asserts = self._ok_assert_results()
        planned = sum(1 for sid in self.steps_by_id if "[" not in sid)
        return {
            "planned": planned,
            "expanded": self.expanded,
            "executed": by_status.get("ok", 0),
            "failed": by_status.get("failed", 0),
            "skipped": by_status.get("skipped", 0),
            "asks": self.asks,
            "autoAsks": self.auto_asks,
            "tests": self._test_run_count(),
            "checks": _count_assert_outcomes(asserts),
            "findings": {
                "deterministic": deterministic,
                "asked": len(self.findings) - deterministic,
                "resolved": self.resolved,
                "dropped": len(self.dropped) + len(self.generic_dropped),
                "refuted": self.refuted,
                "notEmitted": dict(self.not_emitted),
            },
            "truncated": self.truncated,
            "limits": {"planned": self.max_planned, "expanded": self.max_steps},
        }

    def assurance(self) -> list[str]:
        """What the program checked and found to hold: counts, then `holds` sentences."""
        lookups = sum(
            1 for record in self.records if record["op"] in LOOKUPS and record["status"] == "ok"
        )
        asserts = self._ok_assert_results()
        held = [
            result
            for result in asserts
            if result.get("value") is True and not result.get("contradicted")
        ]
        unknown = sum(1 for result in asserts if result.get("value") is None)
        runs = self._test_run_count()
        unknown_clause = f", {unknown} could not be decided" if unknown else ""
        first = (
            f"A review program of {len(self.records)} steps ran against the reviewed commit: "
            f"{lookups} repository lookup{_plural_suffix(lookups)}, "
            f"{runs} test run{_plural_suffix(runs)}, "
            f"{self.asks} small-model question{_plural_suffix(self.asks)}; "
            f"{len(held)} of {len(asserts)} check{_plural_suffix(len(asserts))} held"
            f"{unknown_clause}."
        )
        return [first, *_format_holds_sentences(held)]
