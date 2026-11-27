"""Level calibration, rule parsing, lessons, deduplication, and finding formatting for autoreview."""
from __future__ import annotations

import re
from typing import Any

import autoreview_deep as deep
from review_api import (
    FINDINGS_CONTRACT,
    MAX_FINDINGS_PER_FRAME,
    VERDICT_CONTRACT,
    _text,
    _words,
    similar_claims,
)

MAX_REPLACEMENT_CHARS = 2_000
MAX_REPLACEMENT_SPAN = 30
LEVELS = ("critical", "high", "medium", "low", "nit")
OLD_TO_LEVEL = {"blocker": "critical", "major": "high", "minor": "low"}
LEVEL_TO_OLD = {
    "critical": "blocker",
    "high": "major",
    "medium": "minor",
    "low": "minor",
    "nit": "nit",
}

UNPINNED_SCHEMA: dict[str, Any] = {
    "type": ["object", "null"],
    "properties": {
        "behaviour": {"type": "string"},
        "change": {"type": "string"},
        "closest_test": {"type": ["object", "null"]},
        "mutation": {"type": ["object", "null"]},
    },
}
_CITED = re.compile(r"[\w./-]+\.\w+:\d+|\bline \d+", re.I)

AUTOREVIEW_FINDINGS_CONTRACT: dict[str, Any] = {
    "type": "array",
    "maxItems": MAX_FINDINGS_PER_FRAME,
    "items": {
        "type": "object",
        "properties": {
            **FINDINGS_CONTRACT["items"]["properties"],
            "severity": {"enum": [*LEVELS, *OLD_TO_LEVEL]},
            "scenario": {"type": "string"},
            "unpinned": UNPINNED_SCHEMA,
            "consequence": {"type": "string"},
            "end_line": {"type": ["integer", "null"]},
            "replacement": {"type": ["string", "null"]},
        },
        "required": [*FINDINGS_CONTRACT["items"]["required"], "scenario"],
    },
}

AUTOREVIEW_VERDICT_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {
        **VERDICT_CONTRACT["properties"],
        "verdict": {"enum": ["confirmed", "rejected", "unclear", "uncertain"]},
        "severity": {"enum": [*LEVELS, *OLD_TO_LEVEL]},
        "scenario_holds": {"enum": [True, False, "unknown"]},
        "severity_reason": {"type": "string"},
        "needs": {"type": "array", "maxItems": 3, "items": {"type": "string"}},
        "introduced": {"enum": ["this_change", "before"]},
    },
    "required": [*VERDICT_CONTRACT["required"], "severity", "scenario_holds"],
}

AUTOREVIEW_VERDICT_BATCH_CONTRACT: dict[str, Any] = {
    "type": "array",
    "items": {
        **AUTOREVIEW_VERDICT_CONTRACT,
        "properties": {
            **AUTOREVIEW_VERDICT_CONTRACT["properties"],
            "finding": {"type": "integer"},
        },
        "required": [*AUTOREVIEW_VERDICT_CONTRACT["required"], "finding"],
    },
}

SERIOUS = ("critical", "high")
NEVER_SERIOUS = ("tests",)
SERIOUS_ONLY_IF_FAILS = ("maintainability",)

LESSON_DEMOTE_BELOW = 0.30
MERGE_WINDOW = 5
IDENTICAL = 0.8


def to_level(value: Any, default: str | None = "low") -> str | None:
    """A level from what a model (or an older state file) wrote: a level name or one of the old four names."""
    name = str(value).strip().lower() if value is not None else ""
    if name in LEVELS:
        return name
    return OLD_TO_LEVEL.get(name, default)


def set_level(finding: dict[str, Any], level: str) -> None:
    """Set a finding's level and, in step, its severity on the old scale (what review_api's helpers rank by)."""
    finding["level"] = level
    finding["severity"] = LEVEL_TO_OLD[level]


def level_rank(finding: dict[str, Any]) -> tuple[int, float]:
    level = finding.get("level") or to_level(finding.get("severity")) or "low"
    return (LEVELS.index(level), -float(finding.get("confidence") or 0))


def _parse_closest_test(closest: Any) -> dict[str, Any] | None:
    if not isinstance(closest, dict) or not isinstance(closest.get("path"), str):
        return None
    try:
        line = max(1, int(closest.get("line") or 1))
    except (TypeError, ValueError):
        line = 1
    return {"path": closest["path"].strip(), "line": line}


def _parse_mutation(mutation: Any) -> dict[str, Any] | None:
    if not isinstance(mutation, dict):
        return None
    path = mutation.get("path")
    replacement = mutation.get("replacement")
    if not isinstance(path, str) or not isinstance(replacement, str) or "\n" in replacement:
        return None
    try:
        line = int(mutation.get("line"))
    except (TypeError, ValueError):
        return None
    return {"path": path.strip(), "line": line, "replacement": replacement[:300]}


def parse_rules(raw: Any, finding: dict[str, Any]) -> None:
    """The fields the rules below read, from a finder's or an investigator's finding: `unpinned` and
    `consequence`, bounded and validated."""
    unpinned = raw.get("unpinned") if isinstance(raw, dict) else None
    if isinstance(unpinned, dict):
        out: dict[str, Any] = {
            "behaviour": _text(unpinned.get("behaviour"), 300),
            "change": _text(unpinned.get("change"), 300),
        }
        closest = _parse_closest_test(unpinned.get("closest_test"))
        if closest is not None:
            out["closest_test"] = closest
        mutation = _parse_mutation(unpinned.get("mutation"))
        if mutation is not None:
            out["mutation"] = mutation
        finding["unpinned"] = out

    raw_consequence = raw.get("consequence") if isinstance(raw, dict) else ""
    finding["consequence"] = _text(raw_consequence, 400)


def _dict_str(mapping: dict[str, Any], key: str) -> str:
    value = mapping.get(key)
    return value if isinstance(value, str) else ""


def _generic_tests_reason(finding: dict[str, Any]) -> str | None:
    raw_unpinned = finding.get("unpinned")
    unpinned = raw_unpinned if isinstance(raw_unpinned, dict) else {}
    if len(_dict_str(unpinned, "behaviour")) < 8 or len(_dict_str(unpinned, "change")) < 8:
        return "a tests finding that names no change an existing test would miss"

    raw_mutation = unpinned.get("mutation")
    mutation = raw_mutation if isinstance(raw_mutation, dict) else {}
    finding_file = _dict_str(finding, "file")
    mutated = _dict_str(mutation, "path") or finding_file
    if not deep.is_code(finding_file) or not deep.is_code(mutated):
        return "a tests finding about a configuration, manifest, script or document value"
    return None


def generic_reason(finding: dict[str, Any]) -> str | None:
    """Why a finding is generic and not posted, or None. A tests finding must name the behaviour and the specific
    change to it that no test would notice; an architecture or maintainability finding must show a problem that
    exists now, with its place. (A maintainability nit is kept: it is only ever counted.)"""
    category = finding["category"]
    if category == "tests":
        return _generic_tests_reason(finding)
    if category == "maintainability" and finding.get("level") != "nit":
        consequence = finding.get("consequence") or ""
        if len(consequence) < 12 or not _CITED.search(consequence):
            return "a maintainability finding without a problem that exists now"
    return None


def _holds(value: Any) -> bool | None:
    """A verifier's `scenario_holds` as True, False or None (unknown), however it spelled it."""
    if isinstance(value, bool):
        return value
    text = str(value).strip().lower() if value is not None else ""
    if text in ("true", "yes"):
        return True
    if text in ("false", "no"):
        return False
    return None


def capped_level(level: str, category: str, scenario: str, holds: bool | None = None) -> str:
    """`level` under the rubric's hard rules: critical and high need a concrete scenario; missing tests are never
    above medium; maintainability is above medium only when its scenario was shown to fail."""
    if level not in SERIOUS:
        return level
    if not scenario.strip() or category in NEVER_SERIOUS:
        return "medium"
    if category in SERIOUS_ONLY_IF_FAILS and holds is not True:
        return "medium"
    return level


def finding_kind(finding: dict[str, Any]) -> str:
    """The kind a repository's lessons are keyed by: `fast`, `deep:<lens>`, `structure:<shape>`, `compiled` or
    `hybrid` (a compiled step's id is not a kind)."""
    source = str(finding.get("source") or "fast")
    if source == "deep:structure":
        return f"structure:{finding.get('shape') or 'unknown'}"
    if source.startswith("compiled"):
        return "compiled"
    if source.startswith("hybrid"):
        return "hybrid"
    return source


def _apply_finding_lesson(
    finding: dict[str, Any], lowered: dict[tuple[str, str], dict[str, Any]], low_rank: int
) -> bool:
    if finding.get("blind") == "supported":
        return False
    level = finding.get("level") or to_level(finding["severity"])
    if level in ("critical", "high"):
        return False

    rule = lowered.get((finding["category"], finding_kind(finding)))
    if rule is None:
        return False

    accepted = int(rule.get("accepted") or 0)
    total = accepted + int(rule.get("rejected") or 0)
    if total == 0 or (accepted / total) >= LESSON_DEMOTE_BELOW:
        return False

    finding["lesson"] = f"accepted {accepted} of {total} such findings in the last 180 days"
    if LEVELS.index(level) < low_rank:
        set_level(finding, "low")
        return True
    return False


def apply_lessons(findings: list[dict[str, Any]], lessons: list[dict[str, Any]]) -> int:
    """What this repository taught: a confirmed finding of a category and kind it rarely accepted is posted at
    `low`, with the count beside it. After the verifier and blind check; never flattens high/critical or
    blind-supported findings."""
    lowered = {
        (str(item.get("category")), str(item.get("kind"))): item
        for item in lessons
        if isinstance(item, dict) and item.get("lowered")
    }
    low_rank = LEVELS.index("low")
    return sum(1 for finding in findings if _apply_finding_lesson(finding, lowered, low_rank))


def _final_level(finding: dict[str, Any], level: str, holds: bool | None) -> str:
    if level in SERIOUS and holds is not True:
        return "low"
    if finding["category"] == "tests" and finding.get("proof") != "proven" and not finding.get("test_run"):
        return "nit"
    return capped_level(level, finding["category"], finding.get("scenario") or "", holds)


def final_level(finding: dict[str, Any], verdict: Any) -> str:
    """The level a confirmed finding is posted with: the verifier's own rating (it may raise or lower the
    finder's). Critical and high stand only when the verifier found that the stated scenario really fails;
    otherwise the claimed failure is not shown and the finding is hardening: low."""
    rated = to_level(verdict.get("severity"), None) if isinstance(verdict, dict) else None
    level = rated or finding["level"]
    holds = _holds(verdict.get("scenario_holds")) if isinstance(verdict, dict) else None
    finding["verifier_level"] = rated
    finding["verifier_holds"] = holds if holds is not None else "unknown"
    final = _final_level(finding, level, holds)
    reason = _text(verdict.get("severity_reason"), 1000) if isinstance(verdict, dict) else ""
    if reason and final == rated:
        finding["level_reason"] = reason
    return final


def _extras(raw: dict[str, Any], finding: dict[str, Any]) -> None:
    """The autoreview-only fields of one finder reply item: the level, the scenario, a line range and an exact
    replacement. The finder's own level is kept as `finder_level`; `level` is capped by the rubric's hard rules."""
    finding["scenario"] = _text(raw.get("scenario"), 500)
    finding["finder_level"] = to_level(raw.get("severity"))
    set_level(finding, capped_level(finding["finder_level"], finding["category"], finding["scenario"]))
    parse_rules(raw, finding)

    end = raw.get("end_line")
    line = finding["line"]
    try:
        original = int(raw.get("line"))
    except (TypeError, ValueError):
        original = None
    if original != line:
        return
    if isinstance(end, int) and not isinstance(end, bool) and line < end <= line + MAX_REPLACEMENT_SPAN:
        finding["end_line"] = end
    replacement = raw.get("replacement")
    if isinstance(replacement, str) and replacement.strip() and len(replacement) <= MAX_REPLACEMENT_CHARS:
        finding["replacement"] = replacement.replace("\r\n", "\n").rstrip("\n")


def nearly_identical(first: str, second: str, threshold: float = IDENTICAL) -> bool:
    """Two texts that say the same thing in nearly the same words (unlike `similar_claims`, which accepts half)."""
    left, right = _words(first), _words(second)
    if len(left) < 3 or len(right) < 3:
        return False
    return len(left & right) / min(len(left), len(right)) >= threshold


def _brief_duplicate_item(item: dict[str, Any]) -> dict[str, Any]:
    return {
        "file": item["file"],
        "line": item["line"],
        "category": item.get("category"),
        "claim": _text(item.get("claim"), 160),
        "source": item.get("source") or "fast",
    }


def duplicate_record(dropped: dict[str, Any], into: dict[str, Any], stage: str) -> dict[str, Any]:
    """What was merged into what, for `dropped.duplicateOf`."""
    return {
        "dropped": _brief_duplicate_item(dropped),
        "into": _brief_duplicate_item(into),
        "stage": stage,
    }


def _is_fast_twin(group: dict[str, Any], finding: dict[str, Any]) -> bool:
    if group["category"] != finding["category"]:
        return False
    same_spot = group["file"] == finding["file"] and abs(group["line"] - finding["line"]) <= MERGE_WINDOW
    return same_spot or nearly_identical(group["claim"], finding["claim"])


def _merge_fast_twin(
    twin: dict[str, Any], finding: dict[str, Any], records: list[dict[str, Any]]
) -> None:
    reviewers = twin["reviewers"] + [
        key for key in finding["reviewers"] if key not in twin["reviewers"]
    ]
    if level_rank(finding) < level_rank(twin):
        records.append(duplicate_record(twin, finding, "fast"))
        kept = dict(finding, reviewers=reviewers)
        twin.clear()
        twin.update(kept)
    else:
        records.append(duplicate_record(finding, twin, "fast"))
        twin["reviewers"] = reviewers
    twin["confidence"] = max(twin["confidence"], finding["confidence"])


def dedupe_fast(
    findings: list[dict[str, Any]], records: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    """The fast pass's findings, one per problem: findings in one file within `MERGE_WINDOW` lines of one category
    (several reviewers reading the same slice) merge into the most serious, as do findings of one category whose
    claims are nearly identical. Findings of different categories never merge."""
    merged: list[dict[str, Any]] = []
    ordered = sorted(findings, key=lambda item: (item["file"], item["line"], level_rank(item)))
    for finding in ordered:
        twin = next((group for group in merged if _is_fast_twin(group, finding)), None)
        if twin is None:
            merged.append(dict(finding, reviewers=list(finding["reviewers"])))
        else:
            _merge_fast_twin(twin, finding, records)

    merged.sort(key=level_rank)
    for number, finding in enumerate(merged, 1):
        finding["id"] = number
    return merged


def _evidence_strength_rank(item: dict[str, Any]) -> int:
    if item.get("test_run") or item.get("host_confirmed"):
        return 0
    if item.get("beyond_diff"):
        return 1
    return 2


def _same_root_cause(existing: dict[str, Any], finding: dict[str, Any]) -> bool:
    if (
        existing["file"] == finding["file"]
        and abs(existing["line"] - finding["line"]) <= MERGE_WINDOW
        and similar_claims(existing["claim"], finding["claim"])
    ):
        return True
    if existing["category"] != finding["category"]:
        return False
    if nearly_identical(existing["claim"], finding["claim"]):
        return True
    has_scenarios = bool(existing.get("scenario")) and bool(finding.get("scenario"))
    return (
        has_scenarios
        and nearly_identical(existing["scenario"], finding["scenario"], 0.9)
        and similar_claims(existing["claim"], finding["claim"])
    )


def _merge_root_cause_twin(
    twin: dict[str, Any], finding: dict[str, Any], records: list[dict[str, Any]] | None
) -> None:
    if records is not None:
        records.append(duplicate_record(finding, twin, "root-cause"))
    place = {"file": finding["file"], "line": finding["line"]}
    also_at = twin.setdefault("also_at", [])
    if place not in also_at and len(also_at) < 8:
        also_at.append(place)
    finding_reviewers = finding.get("reviewers")
    if isinstance(finding_reviewers, list):
        for name in finding_reviewers:
            if name not in twin["reviewers"]:
                twin["reviewers"].append(name)
    deep.merge_rules(twin, finding)


def group_root_causes(
    confirmed: list[dict[str, Any]], records: list[dict[str, Any]] | None = None
) -> tuple[list[dict[str, Any]], int]:
    """One finding per root cause: confirmed findings of one category that say the same thing in different places
    (nearly identical claims, or scenarios that describe the same failing input) become the most serious of them,
    with the other places listed in `also_at`. Findings within `MERGE_WINDOW` lines of each other in one file whose
    claims are alike are one finding whatever their categories."""
    kept: list[dict[str, Any]] = []
    merged = 0
    ordered = sorted(
        confirmed,
        key=lambda item: (level_rank(item)[0], _evidence_strength_rank(item), level_rank(item)[1]),
    )
    for finding in ordered:
        twin = next((item for item in kept if _same_root_cause(item, finding)), None)
        if twin is None:
            kept.append(finding)
            continue
        merged += 1
        _merge_root_cause_twin(twin, finding, records)
    return kept, merged


def check_view(check: dict[str, Any]) -> str:
    """What the verifier frame is told about a host-written check that did not decide a candidate."""
    shape = check.get("shape") or "none"
    template = check.get("template") or "no template"
    detail = _text(check.get("detail") or check.get("gate") or "undecided", 240)
    text = f"A host-written check ran first and did not decide this finding (shape {shape}; {template}): {detail}."

    asked = check.get("asked")
    if isinstance(asked, dict) and asked.get("answer"):
        quote_part = f", quoting `{asked['quote']}`" if asked.get("quote") else ""
        why_part = f": {_text(asked['why'], 300)}" if asked.get("why") else ""
        text += (
            f"\nThe small model it asked answered {asked['answer']}"
            f"{quote_part}{why_part}. Untrusted; judge from the source."
        )
    return text


def _public_how_verified(
    finding: dict[str, Any], verification: str, citations: list[dict[str, Any]]
) -> str:
    if finding.get("how_verified"):
        return str(finding["how_verified"])
    if verification != "confirmed":
        detail = _text(finding.get("verification") or "the verifier could not decide", 160)
        return f"not confirmed: {detail}"

    parts: list[str] = []
    if finding.get("test_run"):
        parts.append(f"test run {finding['test_run']} by the host in a sandbox; ")
    if citations:
        where = ", ".join(f"{item['path']}:{item['line']}" for item in citations[:4])
        plural = "" if len(citations) == 1 else "s"
        parts.append(f"{len(citations)} quoted line{plural} checked at the reviewed commit ({where}); ")
    parts.append(f"a verifier confirmed it against the source of {finding['file']}")
    return "".join(parts)


def _public_unpinned(finding: dict[str, Any]) -> dict[str, Any]:
    unpinned = finding["unpinned"]
    result: dict[str, Any] = {
        "behaviour": unpinned.get("behaviour", ""),
        "change": unpinned.get("change", ""),
        "closestTest": unpinned.get("closest_test"),
    }
    if unpinned.get("mutation"):
        result["mutation"] = unpinned["mutation"]
    if finding.get("proof"):
        result["proof"] = finding["proof"]
    return result


def _public_strength(finding: dict[str, Any], citations: list[dict[str, Any]]) -> str:
    if finding.get("test_run"):
        return "test"
    if citations and finding.get("beyond_diff"):
        return "outside"
    return "diff"


_PUBLIC_PASSTHROUGH_FIELDS = (
    ("verified_by", "verifiedBy"),
    ("also_at", "alsoAt"),
    ("consequence", "consequence"),
    ("suggested_fix", "suggestedFix"),
    ("replacement", "replacement"),
    ("shape", "shape"),
    ("lesson", "lesson"),
    ("level_reason", "levelReason"),
    ("blind", "blind"),
)


def _attach_evidence_and_notes(
    out: dict[str, Any],
    finding: dict[str, Any],
    verification: str,
    citations: list[dict[str, Any]],
) -> None:
    cited = "; ".join(f"{item['path']}:{item['line']} `{item['quote']}`" for item in citations)
    evidence = finding.get("evidence")
    if cited or evidence:
        out["evidence"] = " | ".join(part for part in (cited, evidence) if part)
    if verification == "uncertain" and finding.get("verification"):
        out["note"] = finding["verification"]
    if finding.get("rules"):
        out["rules"] = list(finding["rules"])


def _attach_optional_public_fields(
    out: dict[str, Any],
    finding: dict[str, Any],
    verification: str,
    citations: list[dict[str, Any]],
) -> None:
    for src_key, dst_key in _PUBLIC_PASSTHROUGH_FIELDS:
        if finding.get(src_key):
            out[dst_key] = finding[src_key]

    if finding.get("unpinned"):
        out["unpinned"] = _public_unpinned(finding)
    if finding.get("unclear"):
        out["unclear"] = True
    end_line = finding.get("end_line")
    if isinstance(end_line, int) and end_line > finding["line"]:
        out["endLine"] = end_line

    _attach_evidence_and_notes(out, finding, verification, citations)


def _public(finding: dict[str, Any], verification: str) -> dict[str, Any]:
    level = finding.get("level") or to_level(finding["severity"])
    finder_level = finding.get("finder_level") or level
    raw_citations = finding.get("citations")
    citations = raw_citations if isinstance(raw_citations, list) else []
    raw_reviewers = finding.get("reviewers")
    reviewers = list(raw_reviewers) if isinstance(raw_reviewers, list) else []

    out: dict[str, Any] = {
        "id": finding.get("id"),
        "file": finding["file"],
        "line": finding["line"],
        "level": level,
        "severity": LEVEL_TO_OLD[level],
        "category": finding["category"],
        "claim": finding["claim"],
        "why": finding["why"],
        "verification": verification,
        "confidence": round(float(finding["confidence"]), 2),
        "reviewers": reviewers,
        "source": finding.get("source") or "fast",
        "howVerified": _public_how_verified(finding, verification, citations),
        "finderLevel": finder_level,
        "finderSeverity": LEVEL_TO_OLD[finder_level],
        "strength": _public_strength(finding, citations),
        "scenario": finding.get("scenario") or "",
        "kind": finding_kind(finding),
    }

    if citations:
        out["citations"] = citations
    if "verifier_level" in finding:
        out["verifierLevel"] = finding["verifier_level"]
        out["verifierScenarioHolds"] = finding["verifier_holds"]

    _attach_optional_public_fields(out, finding, verification, citations)
    return out
