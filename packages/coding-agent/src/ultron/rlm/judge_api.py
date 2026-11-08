"""`judge`: closed-set decisions from the judge seat (crates/judge): Jev, else a d1 server, else the judge's fallback
model; an unsure answer is escalated to a model of another family, else marked for review.
"""

from __future__ import annotations

import json
from typing import Any

_TYPES = ("choice", "yesno", "score", "number")

#: Questions one `judge.decide` takes at most: crates/judge `question::MAX_QUESTIONS` (a judge test checks they agree).
MAX_QUESTIONS = 16


class Judge:
    """Decisions over evidence, for batch and background work (checking memories, findings, relations), not for every
    turn. `v = await judge.decide(state, questions, kind="memory")`:

    - `state`: the text to judge (a claim and its evidence); a dict or list is sent as JSON.
    - `questions`: `{name: spec}`, 1 to `MAX_QUESTIONS` (16), each with `instructions` and a `type`:
      `{"type": "choice", "criteria": {label: description, ...}, "multiple": False}` (2 to 32 labels),
      `{"type": "yesno"}`, `{"type": "score", "levels": [description, ...]}` (answered 1..n),
      `{"type": "number", "range": [lo, hi], "step": s}`.
    - `kind`: a short name for the decision ("memory", "finding"); it picks the confidence thresholds
      (`judge.thresholds`) and labels the receipts.

    The verdict: `{"answers": {name: {"value", "confidence", "probabilities"?}}, "rung", "settled", "backend",
    "model", "abstained", "review", "reason", "calls", "cost_usd", "receipt", "receipts"}`. `rung` is "first" (sure
    enough), "escalated" (another family's model answered), "review" (still unsure: the answers are the first
    backend's, for a person to settle) or "abstained" (no backend answered: `answers` is empty and `reason` says
    why); `settled` is True for "first" and "escalated", when the answers stand. `receipt` is the id of the call
    that answered (None when none did); a verdict recorded elsewhere (`knowledge`) names it.
    `cost_usd` is what the calls that answered cost (None when one did not report it; failed calls are
    in `receipts`). Every call is a receipt and a session usage row;
    the seat stops at `judge.maxCalls` and `judge.budgetUsd`. Raises ValueError for invalid arguments.

    `expect={name: value}` says what the caller already holds (a verifier's yes): a first answer that differs is a
    dispute, escalated to a model of another family like an unsure one (`reason` starts "disputed")."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def decide(self, state: Any, questions: dict[str, Any], kind: str | None = None,
                     expect: dict[str, Any] | None = None) -> dict[str, Any]:
        """Decide `questions` about `state` (see `help(judge)`); returns the verdict. `expect`: answers already held,
        by question name; one the judge disputes is escalated."""
        if not isinstance(state, str):
            state = json.dumps(state, ensure_ascii=False)
        if not state.strip():
            raise ValueError("judge.decide state must not be empty")
        if not isinstance(questions, dict) or not questions:
            raise ValueError("judge.decide questions must be a non-empty dict of {name: spec}")
        for name, spec in questions.items():
            if not isinstance(spec, dict) or spec.get("type") not in _TYPES:
                raise ValueError(f"judge.decide questions[{name!r}] needs a type: choice, yesno, score or number")
        if kind is not None and not isinstance(kind, str):
            raise ValueError("judge.decide kind must be a string")
        if expect is not None and (not isinstance(expect, dict) or any(name not in questions for name in expect)):
            raise ValueError("judge.decide expect must be {question name: answer}")
        return await self._bridge.request("judge.decide", {"state": state, "questions": questions, "kind": kind,
                                                           "expect": expect})

    def __repr__(self) -> str:
        return "<judge: decide(state, questions, kind=None)>"
