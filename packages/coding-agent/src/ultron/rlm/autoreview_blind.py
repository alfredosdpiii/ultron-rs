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
        "instructions": "Does the code shown (the diff hunk, the source around the line and the quoted source) "
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


async def check(rlm: Any, findings: list[dict[str, Any]], material: Callable[[dict[str, Any]], tuple[str, str] | None]
                ) -> dict[str, Any]:
    """Ask about each of `findings` (confirmed, in order of seriousness) not proven by the host; `material(finding)`
    is its (diff hunk, source window) or None. Sets `finding["blind"]` to supported, refuted or review. Returns the
    tally: {"asked", "supported", "refuted", "review", "calls", "costUsd", "unpricedCalls", "unavailable"}."""
    out: dict[str, Any] = {"asked": 0, "supported": 0, "refuted": 0, "review": 0, "calls": 0, "costUsd": 0.0,
                           "unpricedCalls": 0, "unavailable": None}
    if getattr(rlm, "host_request", None) is None:
        out["unavailable"] = "the host serves no judge"
        return out
    judge = Judge(_Bridge(rlm))
    for finding in [item for item in findings if not item.get("host_confirmed")][:MAX_ASKED]:
        shown = material(finding)
        if shown is None:
            continue
        try:
            verdict = await judge.decide(state(finding, *shown), QUESTIONS, kind=KIND, expect={"shown": True})
        except Exception as error:  # an engine without the judge, or a host failure: the review goes on
            out["unavailable"] = _text(f"{type(error).__name__}: {error}", 160)
            break
        sent = [item for item in verdict.get("receipts") or [] if item.get("status") in ("succeeded", "failed")]
        out["calls"] += len(sent)
        out["unpricedCalls"] += sum(1 for item in sent if item.get("cost_usd") is None)
        out["costUsd"] += sum(float(item.get("cost_usd") or 0) for item in sent)
        rung = verdict.get("rung")
        if rung == "abstained":
            out["unavailable"] = verdict.get("reason") or "the judge abstained"
            break
        out["asked"] += 1
        answer = ((verdict.get("answers") or {}).get("shown") or {}).get("value")
        if verdict.get("settled") and isinstance(answer, bool):
            finding["blind"] = "supported" if answer else "refuted"
        else:
            finding["blind"] = "review"
        out[finding["blind"]] += 1
    out["costUsd"] = round(out["costUsd"], 6)
    return out
