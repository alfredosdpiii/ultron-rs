"""The `yield_after` argument of bash() and tools.call(): how long a call waits before it continues in the host,
and whether its end then arrives as a runtime event."""

from __future__ import annotations

import math
import os

# The longest wait the host takes (its JOB_MAX_WAIT_SECONDS and TOOL_CALL_MAX_WAIT_SECONDS).
MAX_WAIT_SECONDS = 3600
_DEFAULT_SECONDS = 30.0


class _Default:
    """Marker for an argument left out (distinct from an explicit None)."""

    def __repr__(self) -> str:
        return "default"


DEFAULT = _Default()


def async_events_enabled() -> bool:
    return os.environ.get("ULTRON_ASYNC_EVENTS", "").strip().lower() not in ("off", "0", "false", "no")


def setting(*names: str) -> float | None:
    """The first of the environment variables `names` that is set, in seconds (default 30); 0 or "off" makes the
    call block until it ends."""
    raw = next((value for value in (os.environ.get(name, "").strip() for name in names) if value), "").lower()
    if raw == "":
        return _DEFAULT_SECONDS
    if raw in ("off", "none", "false", "no"):
        return None
    try:
        value = float(raw)
    except ValueError:
        return _DEFAULT_SECONDS
    if not math.isfinite(value) or value < 0:
        return _DEFAULT_SECONDS
    return None if value == 0 else min(value, MAX_WAIT_SECONDS)
