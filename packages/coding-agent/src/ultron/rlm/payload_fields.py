"""Host request payloads that leave out the optional fields a call was not given."""

from __future__ import annotations

from typing import Any


def given(**fields: Any) -> dict[str, Any]:
    """`fields` without the ones that are None."""
    return {key: value for key, value in fields.items() if value is not None}
