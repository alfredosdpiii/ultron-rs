"""The blind check of confirmed findings: the judge seat (`judge.decide`, crates/judge) asked one closed question per
finding, seeing only the claim, the diff hunk, the source around the line and the source the verifier quoted. It
never sees the finder's reasoning (why, scenario, suggested fix, confidence) nor the verifier's verdict.

The verifier's confirmation is passed as the answer already held (`expect`): a judge that answers otherwise is a
dispute, which the seat escalates to a model of another family. A finding the escalation also refutes is no longer
confirmed (it is posted as uncertain); one the judge could not settle stays confirmed, marked for review. Findings
the host proved itself (a test it ran, the deterministic stage) are not asked: plain code settled them.

Nothing here fails a review: with no judge backend configured, or once the seat stops (its call ceiling or budget),
the rest are not asked and `unavailable` says why.
"""
from __future__ import annotations

from typing import Any, Callable

from judge_api import Judge
from review_api import _text

KIND = "review.finding"
QUESTIONS: dict[str, Any] = {
    "shown": {
        "type": "yesno",
        "instructions": "Does the code shown (the diff hunk, the source around the line, any cited files, and the quoted source) "
                        "itself show what the claim says? Answer yes only when the code shows it; answer no when the "
                        "claim needs facts the code shown does not contain, or the code shows otherwise.",
    },
}
#: Findings asked about in one review at most (the most serious first).
MAX_ASKED = 12


class _Bridge:
    """`judge_api.Judge` over the engine's host requests."""

    def __init__(self, rlm: Any) -> None:
        self._rlm = rlm

    async def request(self, kind: str, payload: dict[str, Any]) -> Any:
        return await self._rlm.host_request(kind, payload)


def state(finding: dict[str, Any], hunk: str, window: str) -> str:
    """What the judge sees of one finding: the claim and the code, nothing of anyone's reasoning about it."""
    return (f"CLAIM about {finding['file']} line {finding['line']}: {_text(finding['claim'], 400)}\n\n"
            f"DIFF HUNK (new-file line numbers):\n{hunk}\n\n"
            f"SOURCE AROUND THE LINE (> marks it):\n{window}\n\n"
            f"SOURCE THE VERIFIER QUOTED:\n{_text(finding.get('evidence') or '(none)', 500)}")


def _record_receipts(out: dict[str, Any], verdict: dict[str, Any]) -> None:
    receipts = verdict.get("receipts")
    if not receipts:
        return
    for item in receipts:
        if item.get("status") not in ("succeeded", "failed"):
            continue
        out["calls"] += 1
        cost = item.get("cost_usd")
        if cost is None:
            out["unpricedCalls"] += 1
        else:
            out["costUsd"] += float(cost)


def _classify_verdict(verdict: dict[str, Any]) -> str:
    answers = verdict.get("answers")
    shown_entry = answers.get("shown") if isinstance(answers, dict) else None
    answer = shown_entry.get("value") if isinstance(shown_entry, dict) else None
    if verdict.get("settled") and isinstance(answer, bool):
        return "supported" if answer else "refuted"
    return "review"


async def _ask_one(
    judge: Judge,
    finding: dict[str, Any],
    shown: tuple[str, str],
    out: dict[str, Any],
) -> bool:
    try:
        verdict = await judge.decide(state(finding, *shown), QUESTIONS, kind=KIND, expect={"shown": True})
    except Exception as error:  # an engine without the judge, or a host failure: the review goes on
        out["unavailable"] = _text(f"{type(error).__name__}: {error}", 160)
        return False
    _record_receipts(out, verdict)
    if verdict.get("rung") == "abstained":
        out["unavailable"] = verdict.get("reason") or "the judge abstained"
        return False
    out["asked"] += 1
    classification = _classify_verdict(verdict)
    finding["blind"] = classification
    out[classification] += 1
    return True


async def check(
    rlm: Any,
    findings: list[dict[str, Any]],
    material: Callable[[dict[str, Any]], tuple[str, str] | None],
) -> dict[str, Any]:
    """Ask about each confirmed finding not proven by the host.

    `material(finding)` returns `(diff hunk, source window)` or `None`.
    Sets `finding["blind"]` to `supported`, `refuted`, or `review`, and
    returns the tally dict.
    """
    out: dict[str, Any] = {
        "asked": 0,
        "supported": 0,
        "refuted": 0,
        "review": 0,
        "calls": 0,
        "costUsd": 0.0,
        "unpricedCalls": 0,
        "unavailable": None,
    }
    if getattr(rlm, "host_request", None) is None:
        out["unavailable"] = "the host serves no judge"
        return out
    judge = Judge(_Bridge(rlm))
    candidates = [item for item in findings if not item.get("host_confirmed")][:MAX_ASKED]
    for finding in candidates:
        shown = material(finding)
        if shown is None:
            continue
        if not await _ask_one(judge, finding, shown, out):
            break
    out["costUsd"] = round(out["costUsd"], 6)
    return out
