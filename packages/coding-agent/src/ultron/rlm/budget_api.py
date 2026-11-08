"""`budget.estimate`: what planned model work would cost before it starts."""

from __future__ import annotations

from typing import Any


class BudgetEstimator:
    """Cost projection for this session's model work (see `help(budget.estimate)`)."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def estimate(self, plan: Any = None, *, workflow: Any = None) -> dict[str, Any]:
        """Estimate what a plan of model work costs, before running it.

        plan: one unit or a list of units {kind, calls=1, model=None, requests=None, input_tokens=None,
        output_tokens=None}. kind is "child" (a subagent: about 20 requests of 30k input / 1.5k output tokens),
        "frame" (one rlm.infer/rlm.map frame: 1 request, 4k / 500), "chat" (one completion: 1 request, 4k / 1k) or
        "judge" (one judge decision: 1 request, 1k / 20). requests and tokens are per call; what a unit leaves out
        comes from its kind and is listed in the unit's `assumed`. model defaults to the kind's model in this session
        (rlm.childModel or rlm.frameModel, else the session model; a judge has none).
        workflow: the nodes of a workflows.run graph (validated as workflows.run validates it); each node is one unit,
        a revision loop's node and reviewer once per round.

        Returns {"units": [{kind, model, calls, requests, input_tokens, output_tokens, cost_usd, assumed}],
        "cost_usd": total of the priced units, "complete": every unit priced, "unpriced": [{kind, model}],
        "budget": {max_cost_usd, spent_usd, remaining_usd, unknown_priced_calls, fits, max_calls?, calls_used?}}.
        A model without a catalog price is listed in "unpriced" (its cost_usd is None) and left out of the total,
        never guessed; then "fits" is None, as it is without a cost cap. Input is priced at the full input rate (no
        cache discount), so the estimate leans high. With a cost cap (ULTRON_MAX_COST_USD), work whose estimate would
        cross what is left is refused when it starts (workflow nodes), with the numbers in the refusal."""
        if (plan is None) == (workflow is None):
            raise ValueError("budget.estimate takes a plan or workflow=nodes, not both")
        if workflow is not None:
            nodes = workflow.get("nodes") if isinstance(workflow, dict) else workflow
            return await self._bridge.request("budget.estimate", {"workflow": {"nodes": nodes}})
        return await self._bridge.request("budget.estimate", {"plan": plan})
