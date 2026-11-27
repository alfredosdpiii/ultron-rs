"""Shared review pipeline helpers for `review_api` and `autoreview_api`."""
from __future__ import annotations

import json
from typing import Any, Callable, Sequence


def parse_reviewer_keys(
    only: Any,
    *,
    reviewers: Sequence[str],
    aliases: dict[str, str],
    error_cls: type[Exception],
    require_non_empty: bool = False,
) -> list[str]:
    """Normalize reviewer names/aliases into deduplicated canonical keys."""
    if not only:
        if require_non_empty:
            raise error_cls("--only needs at least one reviewer")
        return list(reviewers)
    raw_items = only if isinstance(only, list) else str(only).split(",")
    keys: list[str] = []
    for raw in raw_items:
        name = str(raw).strip().lower()
        if not name:
            continue
        key = aliases.get(name, name)
        if key not in reviewers:
            raise error_cls(f"unknown reviewer {str(raw).strip()!r}; choose from {', '.join(reviewers)}")
        if key not in keys:
            keys.append(key)
    if not keys:
        if require_non_empty:
            raise error_cls("--only needs at least one reviewer")
        return list(reviewers)
    return keys


def find_frame_input(
    reviewer: Any,
    chunk_text: str,
    *,
    conventions_key: str,
    reviewer_cls: type[Any],
    finder_task: Callable[[Any], str],
) -> tuple[str, str | list[str]]:
    """Build the `(task, item)` pair for a finder frame, placing convention files in a dedicated view."""
    if reviewer.key == conventions_key and reviewer.checklist:
        slim = reviewer_cls(
            reviewer.key,
            reviewer.title,
            reviewer.focus,
            (),
            only_paths=reviewer.only_paths,
        )
        conv_view = "Repository and directory convention files:\n" + "\n\n".join(reviewer.checklist)
        return finder_task(slim), [conv_view, chunk_text]
    return finder_task(reviewer), chunk_text


def build_verify_context(
    finding: dict[str, Any],
    *,
    window: str,
    hunk_text: str,
    related: str,
    public_keys: Sequence[str] = ("file", "line", "severity", "category", "claim", "why", "suggested_fix"),
    severity_override: Any = None,
    conventions_blocks: Sequence[str] = (),
    contract_lines: Sequence[str] = (),
    read_code: str = "",
    extra_views: Sequence[str] = (),
    extra_sources: Sequence[str] = (),
) -> tuple[dict[str, Any], list[str], str]:
    """Assemble the public finding payload, verifier views, and quote-verification source text."""
    public = {key: finding[key] for key in public_keys}
    if severity_override is not None:
        public["severity"] = severity_override
    views = [
        f"Finding:\n{json.dumps(public, indent=1)}",
        f"Source of {finding['file']} around line {finding['line']} (> marks the cited line):\n{window}",
        f"Diff hunk ({finding['file']}, new-file line numbers):\n{hunk_text}",
    ]
    if related:
        views.append(f"Other places that define or use the names involved:\n{related}")
    if contract_lines:
        views.append(
            "What the endpoints this file talks to answer, and their other callers (found by a search "
            "of the repository):\n" + "\n".join(contract_lines)
        )
    conv_text = ""
    if conventions_blocks:
        conv_text = "\n\n".join(conventions_blocks)
        views.append(f"Repository and directory convention files governing this file:\n{conv_text}")
    if read_code:
        views.append(f"Code outside the diff that the reviewer who raised this read:\n{read_code}")
    views.extend(extra_views)
    all_sources = (window, hunk_text, related, *extra_sources, conv_text, read_code)
    source_parts = [part for part in all_sources if part]
    return public, views, "\n".join(source_parts)
