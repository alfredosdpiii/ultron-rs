"""Finding normalization, deduplication, evidence checks, verdict application, and symbol/code lookups."""
from __future__ import annotations

import re
from typing import Any

from infer_api import FrameError, Incomplete
from review_prompts import Reviewer
from review_api_types import (
    CONTEXT_LINES,
    DEDUPE_WINDOW,
    MAX_FINDINGS_PER_FRAME,
    NEED_CHARS,
    NEED_LINES,
    RELATED_CHARS,
    SEVERITIES,
    VERIFY_WINDOW,
    _CALL,
    _CATEGORY_WORDS,
    _COMMON,
    _DEF,
    _DEFINITION_WORDS,
    _NEED_NAME,
    _REVIEWER_CATEGORY,
    Chunk,
    Git,
    Scope,
    _clip,
    _git_grep_lines,
    _normalize,
    _rank,
    _source_bodies,
    _text,
    _words,
)


def normalize_category(category: str, reviewer: str | None = None) -> str:
    text = (category or "").strip().lower()
    for key, words in _CATEGORY_WORDS.items():
        if text == key or any(word in text for word in words):
            return key
    return _REVIEWER_CATEGORY.get(reviewer or "", text or "other")


def _parse_finding_line(raw_line: Any, default_line: int, upper_bound: int) -> int:
    try:
        line = int(raw_line)
    except (TypeError, ValueError):
        return default_line
    return line if 1 <= line <= max(upper_bound, 1) else default_line


def _parse_finding_confidence(raw_confidence: Any) -> float:
    try:
        confidence = float(raw_confidence)
    except (TypeError, ValueError):
        confidence = 0.5
    return min(1.0, max(0.0, confidence))


def _normalize_finding_item(
    item: Any,
    reviewer: Reviewer,
    chunk: Chunk,
    default_line: int,
    upper_bound: int,
) -> dict[str, Any] | None:
    if not isinstance(item, dict):
        return None
    claim = _text(item.get("claim"), 1000)
    if not claim:
        return None
    severity = str(item.get("severity", "")).strip().lower()
    return {
        # A frame saw one file; a different path in its reply is a slip, not a second file.
        "file": chunk.path,
        "line": _parse_finding_line(item.get("line"), default_line, upper_bound),
        "severity": severity if severity in SEVERITIES else "minor",
        "category": normalize_category(_text(item.get("category"), 40), reviewer.key),
        "claim": claim,
        "why": _text(item.get("why"), 600),
        "suggested_fix": _text(item.get("suggested_fix"), 500),
        "confidence": _parse_finding_confidence(item.get("confidence", 0.5)),
        "reviewers": [reviewer.key],
        "chunk": chunk.id,
    }


def normalize_findings(raw: Any, reviewer: Reviewer, chunk: Chunk, line_count: int | None) -> list[dict[str, Any]]:
    """Validate a finder frame's reply: pin the file to the chunk's, keep lines in range, bound every string."""
    if not isinstance(raw, list):
        return []
    default_line = min(hunk.new_first for hunk in chunk.hunks) if chunk.hunks else 1
    max_hunk_line = max(hunk.new_last for hunk in chunk.hunks) if chunk.hunks else 1
    upper_bound = line_count if line_count else max_hunk_line + CONTEXT_LINES
    out: list[dict[str, Any]] = []
    for item in raw[:MAX_FINDINGS_PER_FRAME]:
        normalized = _normalize_finding_item(item, reviewer, chunk, default_line, upper_bound)
        if normalized is not None:
            out.append(normalized)
    return out


def similar_claims(left_claim: str, right_claim: str) -> bool:
    """Two claims about the same problem in different words: at least half of the shorter claim's words recur."""
    left, right = _words(left_claim), _words(right_claim)
    if len(left) < 3 or len(right) < 3:
        return False
    return len(left & right) / min(len(left), len(right)) >= 0.5


def _matching_group(
    merged: list[dict[str, Any]],
    finding: dict[str, Any],
    window: int,
) -> dict[str, Any] | None:
    for group in merged:
        if group["file"] != finding["file"] or abs(group["line"] - finding["line"]) > window:
            continue
        if group["category"] != finding["category"] and not similar_claims(group["claim"], finding["claim"]):
            continue
        return group
    return None


def _merge_finding_into_group(group: dict[str, Any], finding: dict[str, Any]) -> None:
    reviewers = group["reviewers"] + [key for key in finding["reviewers"] if key not in group["reviewers"]]
    duplicates = group.get("duplicates", 1) + 1
    if _rank(finding) < _rank(group):
        group.clear()
        group.update(finding)
    group["reviewers"] = reviewers
    group["duplicates"] = duplicates
    group["confidence"] = max(group["confidence"], finding["confidence"])


def dedupe(findings: list[dict[str, Any]], window: int = DEDUPE_WINDOW) -> list[dict[str, Any]]:
    """Merge findings on the same file within `window` lines that share a category or make the same claim.
    The merged finding keeps the most severe (then most confident) wording and every reviewer that raised it."""
    merged: list[dict[str, Any]] = []
    for finding in sorted(findings, key=lambda item: (item["file"], item["line"], _rank(item))):
        group = _matching_group(merged, finding, window)
        if group is not None:
            _merge_finding_into_group(group, finding)
        else:
            merged.append(dict(finding, reviewers=list(finding["reviewers"])))
    merged.sort(key=_rank)
    for number, finding in enumerate(merged, 1):
        finding["id"] = number
    return merged


def symbols_for(source: list[str] | None, line: int, limit: int = 3) -> list[str]:
    """Names worth searching for around a cited line: its enclosing definition and what the line calls."""
    if not source:
        return []
    names: list[str] = []
    for index in range(min(line, len(source)) - 1, max(-1, line - 80), -1):
        match = _DEF.match(source[index])
        if match:
            names.append(match.group(1) or match.group(2))
            break
    if 1 <= line <= len(source):
        for name in _CALL.findall(source[line - 1]):
            if name not in _COMMON and name not in names:
                names.append(name)
    return names[:limit]


def _evidence_matches_body(evidence_n: str, bodies: list[str]) -> bool:
    for body in bodies:
        if len(body) >= 6 and re.search(r"[A-Za-z0-9]", body) and body in evidence_n:
            return True
    return False


def _evidence_matches_snippet(evidence: str, source_n: str) -> bool:
    for snippet in re.findall(r"`([^`]{6,})`|\"([^\"]{6,})\"|'([^']{6,})'", evidence):
        text = _normalize(next(part for part in snippet if part))
        if text and text in source_n:
            return True
    return False


def evidence_quotes_source(evidence: str, source_text: str) -> bool:
    """True when the evidence quotes the source: a source line (6+ chars) appears in it, or a quoted or
    backticked snippet (6+ chars) from it appears in the source."""
    evidence_n = _normalize(evidence)
    bodies = _source_bodies(source_text)
    if not evidence_n or not bodies:
        return False
    if _evidence_matches_body(evidence_n, bodies):
        return True
    return _evidence_matches_snippet(evidence, "\n".join(bodies))


def _populate_verdict_metadata(
    finding: dict[str, Any],
    verdict: dict[str, Any],
    line_count: int | None,
) -> str:
    evidence = _text(verdict.get("evidence"), 500)
    finding["evidence"] = evidence
    introduced = verdict.get("introduced")
    if introduced in ("this_change", "before"):
        finding["introduced"] = introduced
    corrected = verdict.get("corrected_line")
    if isinstance(corrected, int) and not isinstance(corrected, bool) and corrected >= 1:
        if line_count is None or corrected <= line_count:
            finding["line"] = corrected
    return evidence


def _route_verdict_outcome(
    finding: dict[str, Any],
    verdict: Any,
    source: str,
    line_count: int | None,
    confirmed: list[dict[str, Any]],
    uncertain: list[dict[str, Any]],
    rejected: list[dict[str, Any]],
) -> None:
    if isinstance(verdict, (Incomplete, FrameError)) or not isinstance(verdict, dict):
        status = getattr(verdict, "status", None) or getattr(verdict, "error", None) or "no verdict"
        finding["verification"] = f"not verified ({_text(status, 120)})"
        uncertain.append(finding)
        return
    evidence = _populate_verdict_metadata(finding, verdict, line_count)
    kind = verdict.get("verdict")
    if kind == "rejected":
        rejected.append(finding)
    elif kind == "confirmed" and evidence_quotes_source(evidence, source):
        confirmed.append(finding)
    elif kind == "confirmed":
        finding["verification"] = "the verifier confirmed it without quoting the source"
        uncertain.append(finding)
    else:
        finding["verification"] = evidence or "the verifier could not decide"
        uncertain.append(finding)


def apply_verdicts(
    findings: list[dict[str, Any]],
    verdicts: list[Any],
    sources: list[str],
    line_counts: list[int | None],
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    """Split verified findings into (confirmed, uncertain, rejected). Rejected findings never reach the report."""
    confirmed: list[dict[str, Any]] = []
    uncertain: list[dict[str, Any]] = []
    rejected: list[dict[str, Any]] = []
    for finding, verdict, source, count in zip(findings, verdicts, sources, line_counts):
        _route_verdict_outcome(dict(finding), verdict, source, count, confirmed, uncertain, rejected)
    return confirmed, uncertain, rejected


def _grep_related_symbol(
    git: Git,
    scope: Scope,
    finding: dict[str, Any],
    name: str,
    size: int,
) -> tuple[list[str], int]:
    hits = _git_grep_lines(git, scope, ["-n", "-w", "-I", "--max-count=4", "-e", name], limit=8)
    entries: list[str] = []
    for path, number, text in hits:
        if path == finding["file"] and abs(number - finding["line"]) <= VERIFY_WINDOW:
            continue
        entry = f"{path}:{number}: {_clip(text.strip(), 200)}"
        if size + len(entry) > RELATED_CHARS:
            break
        entries.append(entry)
        size += len(entry)
    return entries, size


def related_code(git: Git, scope: Scope, finding: dict[str, Any], source: list[str] | None) -> str:
    """Where the names around a finding are defined or used elsewhere (git grep), bounded."""
    if scope.grep_rev == "":
        return ""
    lines: list[str] = []
    size = 0
    for name in symbols_for(source, finding["line"]):
        added, size = _grep_related_symbol(git, scope, finding, name, size)
        lines.extend(added)
    return "\n".join(lines)


def _resolve_need_places(git: Git, scope: Scope, raw_need: Any) -> list[tuple[str, int]]:
    if not isinstance(raw_need, str):
        return []
    need, _, line = raw_need.strip().strip("`").split("(")[0].partition(":")
    if need and scope.read_file(need):
        start_line = line.split("-", 1)[0]
        return [(need, max(1, int(start_line) - 10) if start_line.isdigit() else 1)]
    name = need.rsplit(".", 1)[-1]
    if not _NEED_NAME.fullmatch(name) or scope.grep_rev == "":
        return []
    pattern = rf"({_DEFINITION_WORDS})[[:space:]]+{name}([^[:alnum:]_]|$)"
    hits = _git_grep_lines(git, scope, ["-n", "-I", "-E", "-e", pattern], limit=2)
    return [(path, number) for path, number, _ in hits]


def _format_need_snippet(scope: Scope, path: str, at: int) -> str:
    source = scope.read_file(path) or []
    return "\n".join(
        f" {number:>5} | {_clip(source[number - 1])}"
        for number in range(at, min(len(source), at + NEED_LINES - 1) + 1)
    )


def needed_code(git: Git, scope: Scope, needs: Any) -> str:
    """The code a reviewer named: a file from its start or around `path:line`, or a definition found by name,
    numbered."""
    if not isinstance(needs, list):
        return ""
    out: list[str] = []
    size = 0
    for need in needs[:5]:
        for path, at in _resolve_need_places(git, scope, need):
            body = _format_need_snippet(scope, path, at)
            if body and size + len(body) <= NEED_CHARS:
                size += len(body)
                out.append(f"{path}:{at}\n{body}")
    return "\n\n".join(out)


__all__ = [name for name in globals() if not name.startswith("__")]
