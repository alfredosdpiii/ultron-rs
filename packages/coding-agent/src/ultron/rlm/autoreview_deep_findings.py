"""Finding citation checks, normalization, deduplication, test proofs, and assurance."""
from __future__ import annotations

import re
from typing import Any, Callable

import autoreview_tests as testing
from autoreview_deep_brief import Brief
from autoreview_deep_repo import (
    CITATION_SLACK,
    CITED_WINDOW,
    LEVEL_ORDER,
    MAX_CITATIONS,
    MAX_LEADS,
    MERGE_WINDOW,
    SEVERITY_BY_LEVEL,
    Rejected,
    Repo,
    _flat,
    _list,
    _loose,
    _numbered,
)
from review_api import _TEST, _clip, _text, normalize_category


def merge_rules(into: dict[str, Any], other: dict[str, Any]) -> None:
    """Credit rules cited by `other` onto `into` when `into` supersedes it."""
    other_rules = other.get("rules")
    if not other_rules:
        return
    existing = into.get("rules")
    base = set(existing) if existing else set()
    into["rules"] = sorted(base | set(other_rules))


def _quote_matches(line_text: str, quote: str, normalize: Callable[[str], str]) -> bool:
    normal_line = normalize(line_text)
    if quote in normal_line:
        return True
    return len(normal_line) >= 8 and normal_line in quote


def _find_quote_line(lines: list[str], target_line: int, quote: str) -> int | None:
    low = max(1, target_line - CITATION_SLACK)
    high = min(len(lines), target_line + CITATION_SLACK)
    for number in range(low, high + 1):
        if _quote_matches(lines[number - 1], quote, _flat):
            return number
    loose_quote = _loose(quote)
    for number in range(low, high + 1):
        if _quote_matches(lines[number - 1], loose_quote, _loose):
            return number
    return None


def _check_one_citation(
    repo: Repo, item: Any
) -> tuple[dict[str, Any] | None, str | None]:
    if not isinstance(item, dict):
        return None, "a citation is not an object"
    try:
        path = repo.path(item.get("path"))
        line = int(item.get("line"))
    except (Rejected, TypeError, ValueError) as error:
        return None, f"a citation names no tracked file and line ({error})"

    raw_quote = item.get("quote")
    quote = _flat(raw_quote if isinstance(raw_quote, str) else "")
    lines = repo.lines(path)
    if len(quote) < 4 or not lines:
        return None, f"the citation of {path}:{line} quotes nothing"

    matched = _find_quote_line(lines, line, quote)
    if matched is None:
        return None, f"{path}:{line} does not say {_clip(quote, 80)!r}"
    cleaned = _clip(lines[matched - 1].strip(), 240)
    return {"path": path, "line": matched, "quote": cleaned}, None


def check_citations(
    repo: Repo, raw: Any, diff_lines: dict[str, set[int]]
) -> tuple[list[dict[str, Any]], str | None, bool]:
    """Check a finding's citations against the head commit within CITATION_SLACK lines.
    Returns (citations, failure_reason_or_none, beyond_diff)."""
    items = raw if isinstance(raw, list) else []
    citations: list[dict[str, Any]] = []
    beyond = False
    for item in items[:MAX_CITATIONS]:
        citation, problem = _check_one_citation(repo, item)
        if problem is not None or citation is None:
            return [], problem, False
        citations.append(citation)
        if citation["line"] not in diff_lines.get(citation["path"], ()):
            beyond = True
    if not citations:
        return [], "no evidence", False
    return citations, None, beyond


def _matching_test_run(
    raw: dict[str, Any], runs: list[dict[str, Any]] | None
) -> dict[str, Any] | None:
    number = raw.get("test_run")
    if runs is None or not isinstance(number, int) or isinstance(number, bool):
        return None
    for item in runs:
        if item["n"] == number and item["status"] != "unavailable":
            return item
    return None


def _resolve_stated_level(
    raw: dict[str, Any], to_level: Callable[[Any], str | None] | None
) -> str:
    name = str(raw.get("severity", "")).strip().lower()
    if to_level is not None:
        return to_level(name) or "low"
    return name if name in LEVEL_ORDER else "low"


def _parse_confidence(raw: dict[str, Any]) -> float:
    try:
        return min(1.0, max(0.0, float(raw.get("confidence", 0.6))))
    except (TypeError, ValueError):
        return 0.6


def _extract_cited_rules(raw: dict[str, Any], served_rules: set[int]) -> list[int]:
    raw_rules = raw.get("rules")
    if not isinstance(raw_rules, list):
        return []
    cited: set[int] = set()
    for item in raw_rules:
        if isinstance(item, str) and item.startswith("R") and item[1:].isdigit():
            rule_id = int(item[1:])
            if rule_id in served_rules:
                cited.add(rule_id)
    return sorted(cited)


def _validate_finding_location(
    repo: Repo, raw: dict[str, Any]
) -> tuple[str | None, int, str | None]:
    try:
        path = repo.path(raw.get("file"))
        line = int(raw.get("line"))
    except (Rejected, TypeError, ValueError) as error:
        return None, 0, f"no tracked file and line ({error})"
    lines = repo.lines(path)
    if not lines or not 1 <= line <= len(lines):
        return None, 0, f"{path} has no line {line}"
    return path, line, None


def _attach_optional_fields(
    finding: dict[str, Any],
    raw: dict[str, Any],
    run: dict[str, Any] | None,
    repo: Repo,
    enrich: Callable[[Any, dict[str, Any]], None] | None,
) -> None:
    if run is not None:
        finding["test_run"] = run["n"]
        finding["test_evidence"] = testing.summarize(run).split("\n  output")[0]
    cited = _extract_cited_rules(raw, repo.served_rules)
    if cited:
        finding["rules"] = cited
    if enrich is not None:
        enrich(raw, finding)


def normalize_deep(
    repo: Repo,
    raw: Any,
    lens: str,
    diff_lines: dict[str, set[int]],
    cap: Callable[[str, str, str], str],
    runs: list[dict[str, Any]] | None = None,
    to_level: Callable[[Any], str | None] | None = None,
    enrich: Callable[[Any, dict[str, Any]], None] | None = None,
) -> tuple[dict[str, Any] | None, str | None]:
    """One investigator finding, validated: (finding, None) or (None, why it was dropped)."""
    if not isinstance(raw, dict):
        return None, "not an object"
    claim = _text(raw.get("claim"), 300)
    if not claim:
        return None, "no claim"

    path, line, loc_error = _validate_finding_location(repo, raw)
    if loc_error is not None or path is None:
        return None, loc_error

    citations, problem, beyond = check_citations(repo, raw.get("evidence"), diff_lines)
    # A test run the host made can stand in for quoted lines; an unavailable run proves nothing.
    run = _matching_test_run(raw, runs)
    if problem and (run is None or problem != "no evidence"):
        return None, problem

    stated = _resolve_stated_level(raw, to_level)
    default_cat = "tests" if lens == "tests" else "correctness"
    category = normalize_category(_text(raw.get("category"), 40), default_cat)
    scenario = _text(raw.get("scenario"), 500)
    level = cap(stated, category, scenario)
    finding: dict[str, Any] = {
        "file": path,
        "line": line,
        "level": level,
        "severity": SEVERITY_BY_LEVEL[level],
        "finder_level": stated,
        "category": category,
        "claim": claim,
        "why": _text(raw.get("why"), 600),
        "scenario": scenario,
        "suggested_fix": _text(raw.get("suggested_fix"), 500),
        "confidence": _parse_confidence(raw),
        "reviewers": [f"deep:{lens}"],
        "source": f"deep:{lens}",
        "citations": citations,
        "beyond_diff": beyond or run is not None,
    }
    _attach_optional_fields(finding, raw, run, repo, enrich)
    return finding, None


def cited_windows(repo: Repo, finding: dict[str, Any]) -> str:
    """The source around each citation of a finding, as the host reads it at the head commit."""
    parts: list[str] = []
    raw_citations = finding.get("citations")
    citations = raw_citations if isinstance(raw_citations, list) else []
    for citation in citations:
        lines = repo.lines(citation["path"])
        if lines:
            start = max(1, citation["line"] - CITED_WINDOW)
            end = min(len(lines), citation["line"] + CITED_WINDOW)
            header = f"{citation['path']} (cited line {citation['line']}):"
            parts.append(f"{header}\n{_numbered(lines, start, end)}")
    return "\n\n".join(parts)


def _nearly_identical(left_text: str, right_text: str) -> bool:
    left = {word for word in re.findall(r"[a-z0-9_]+", left_text.lower()) if len(word) > 2}
    right = {word for word in re.findall(r"[a-z0-9_]+", right_text.lower()) if len(word) > 2}
    if len(left) < 3 or len(right) < 3:
        return False
    return len(left & right) / min(len(left), len(right)) >= 0.8


def _brief_record(item: dict[str, Any]) -> dict[str, Any]:
    return {
        "file": item["file"],
        "line": item["line"],
        "category": item.get("category"),
        "claim": _text(item.get("claim"), 160),
        "source": item.get("source") or "fast",
    }


def _record(
    records: list[dict[str, Any]] | None,
    dropped: dict[str, Any],
    into: dict[str, Any],
    stage: str,
) -> None:
    if records is None:
        return
    entry = {"dropped": _brief_record(dropped), "into": _brief_record(into), "stage": stage}
    records.append(entry)


def _same_finding(first: dict[str, Any], second: dict[str, Any]) -> bool:
    if first["category"] != second["category"] or first["file"] != second["file"]:
        return False
    if _nearly_identical(first["claim"], second["claim"]):
        return True
    # One investigator's two findings on neighbouring lines are two findings.
    return (
        abs(first["line"] - second["line"]) <= MERGE_WINDOW
        and first.get("source") != second.get("source")
    )


def _merge_reviewers(target: dict[str, Any], source: dict[str, Any]) -> None:
    reviewers = source.get("reviewers")
    if not isinstance(reviewers, list):
        return
    for name in reviewers:
        if name not in target["reviewers"]:
            target["reviewers"].append(name)


def _citation_count(item: dict[str, Any]) -> int:
    citations = item.get("citations")
    return len(citations) if isinstance(citations, list) else 0


def _dedupe_deep(
    deep: list[dict[str, Any]], records: list[dict[str, Any]] | None
) -> tuple[list[dict[str, Any]], int]:
    order = {level: index for index, level in enumerate(LEVEL_ORDER)}
    ordered = sorted(deep, key=lambda item: (order[item["level"]], -_citation_count(item)))
    kept: list[dict[str, Any]] = []
    duplicates = 0
    for finding in ordered:
        twin = next((item for item in kept if _same_finding(item, finding)), None)
        if twin is None:
            kept.append(finding)
            continue
        duplicates += 1
        _record(records, finding, twin, "deep")
        _merge_reviewers(twin, finding)
        merge_rules(twin, finding)
    return kept, duplicates


def _fold_in_diff_deep(
    kept: list[dict[str, Any]],
    fast: list[dict[str, Any]],
    records: list[dict[str, Any]] | None,
) -> int:
    duplicates = 0
    for finding in [item for item in kept if not item.get("beyond_diff")]:
        other = next((item for item in fast if _same_finding(finding, item)), None)
        if other is not None:
            kept.remove(finding)
            merge_rules(other, finding)
            duplicates += 1
            _record(records, finding, other, "deep-into-fast")
    return duplicates


def _copy_replacement(target: dict[str, Any], source: dict[str, Any]) -> None:
    if (
        not target.get("replacement")
        and source.get("replacement")
        and target["line"] == source["line"]
    ):
        target["replacement"] = source["replacement"]
        if source.get("end_line"):
            target["end_line"] = source["end_line"]


def _merge_fast_into_deep(
    fast: list[dict[str, Any]],
    kept: list[dict[str, Any]],
    records: list[dict[str, Any]] | None,
) -> tuple[list[dict[str, Any]], int]:
    remaining: list[dict[str, Any]] = []
    duplicates = 0
    for finding in fast:
        twin = next((item for item in kept if _same_finding(item, finding)), None)
        if twin is None:
            remaining.append(finding)
            continue
        duplicates += 1
        _record(records, finding, twin, "fast-into-deep")
        _merge_reviewers(twin, finding)
        _copy_replacement(twin, finding)
    return remaining, duplicates


def merge(
    fast: list[dict[str, Any]],
    deep: list[dict[str, Any]],
    records: list[dict[str, Any]] | None = None,
) -> tuple[list[dict[str, Any]], int]:
    """Deduplicate deep findings across investigators and against the fast pass.
    Deep findings with evidence outside the diff supersede matching fast findings;
    in-diff deep findings yield to matching fast findings. Returns (all, duplicates)."""
    kept, deep_dupes = _dedupe_deep(deep, records)
    in_diff_dupes = _fold_in_diff_deep(kept, fast, records)
    remaining, fast_dupes = _merge_fast_into_deep(fast, kept, records)
    return remaining + kept, deep_dupes + in_diff_dupes + fast_dupes


def _test_line(repo: Repo, test_id: str) -> tuple[str | None, int]:
    """The file and line of a test from its id (`path::name`, or a path)."""
    path, _, name = test_id.partition("::")
    try:
        path = repo.path(path)
    except Rejected:
        return None, 1
    leaf = re.split(r"[\[:]", name.split("::")[-1])[0] if name else ""
    lines = repo.lines(path)
    if not lines:
        return path, 1
    for number, line in enumerate(lines, 1):
        if leaf and re.search(rf"\b{re.escape(leaf)}\b", line):
            return path, number
    return path, 1


def _has_unpinned_mutation(item: dict[str, Any]) -> bool:
    if item.get("category") != "tests" or item.get("proof"):
        return False
    unpinned = item.get("unpinned")
    return isinstance(unpinned, dict) and bool(unpinned.get("mutation"))


def _unpinned_candidates(findings: list[dict[str, Any]]) -> list[dict[str, Any]]:
    order = {level: index for index, level in enumerate(LEVEL_ORDER)}
    candidates = [item for item in findings if _has_unpinned_mutation(item)]
    return sorted(candidates, key=lambda item: order.get(item.get("level", "low"), 3))


def _mutation_paths(
    unpinned: dict[str, Any], session: "testing.TestSession", brief: Brief
) -> list[str]:
    closest_info = unpinned.get("closest_test")
    closest = closest_info.get("path") if isinstance(closest_info, dict) else None
    if closest in session.tracked and _TEST.search(closest or ""):
        return [closest]
    return brief.tests[:2]


def _has_failing_auto_run(session: "testing.TestSession", paths: list[str]) -> bool:
    path_set = set(paths)
    return any(
        record["kind"] == "automatic"
        and record["status"] != "passed"
        and path_set & set(record["paths"])
        for record in session.records
    )


def _prove_one_unpinned(
    session: "testing.TestSession", finding: dict[str, Any], brief: Brief
) -> None:
    unpinned = finding["unpinned"]
    paths = _mutation_paths(unpinned, session, brief)
    if not paths:
        return
    # A suite that already fails says nothing about the mutant.
    already_failing = _has_failing_auto_run(session, paths)
    mutation = unpinned["mutation"]
    try:
        record = session.mutation(mutation["path"], mutation["line"], mutation["replacement"], paths)
    except testing.TestsRejected:
        return
    if record["status"] == "passed":
        finding.update(
            proof="proven",
            test_run=record["n"],
            beyond_diff=True,
            test_evidence=testing.summarize(record).split("\n  output")[0],
        )
    elif record["status"] == "failed" and not already_failing:
        finding["proof"] = "refuted"


def prove_unpinned(
    session: "testing.TestSession | None",
    repo: Repo,
    findings: list[dict[str, Any]],
    brief: Brief,
) -> None:
    """Settle tests findings by running one-line mutation replacements against the nearest
    tests, marking each finding `proof` as 'proven' or 'refuted' while runs remain."""
    if session is None:
        return
    for finding in _unpinned_candidates(findings):
        if len(session.records) >= session.limit:
            break
        _prove_one_unpinned(session, finding, brief)


def regression_findings(repo: Repo, compared: dict[str, Any]) -> list[dict[str, Any]]:
    """Build host-confirmed findings for tests that fail at head and passed at base."""
    out: list[dict[str, Any]] = []
    for item in compared["regressions"][:5]:
        head, base = item["head"], item["base"]
        path, line = _test_line(repo, item["id"])
        if path is None:
            continue
        detail = _text(item.get("detail") or "", 240)
        failure_note = f" Failure: {detail}" if detail else ""
        detail_suffix = f": {detail}" if detail else ""
        scenario = f"Running {item['id']} at the reviewed commit fails; at the base commit it passes."
        out.append(
            {
                "file": path,
                "line": line,
                "level": "high",
                "severity": "major",
                "finder_level": "high",
                "category": "correctness",
                "claim": f"{item['id']} fails at this commit and passed at the base commit.",
                "why": f"The host ran the test at both commits in a sandbox.{failure_note}",
                "scenario": scenario,
                "suggested_fix": "",
                "confidence": 0.95,
                "reviewers": ["deep:test-run"],
                "source": "deep:test-run",
                "citations": [],
                "beyond_diff": True,
                "host_confirmed": True,
                "test_run": head["n"],
                "evidence": (
                    f"`{head['command']}` failed at the reviewed commit (run {head['n']}) "
                    f"and passed at the base commit (run {base['n']}){detail_suffix}"
                ),
                "how_verified": (
                    f"the host ran it in a sandbox at the reviewed commit (failed, run {head['n']}) "
                    f"and at the base commit (passed, run {base['n']})"
                ),
            }
        )
    return out


def _collect_refutation_claims(outcomes: list[dict[str, Any]]) -> list[tuple[str, dict[str, Any]]]:
    claims: list[tuple[str, dict[str, Any]]] = []
    for outcome in outcomes:
        if outcome["record"]["status"] == "failed":
            continue
        for item in _list(outcome["reply"].get("refuted")):
            if isinstance(item, dict):
                claims.append((outcome["lens"], item))
    return claims


def refuted_leads(
    repo: Repo,
    outcomes: list[dict[str, Any]],
    leads: list[dict[str, Any]],
    diff_lines: dict[str, set[int]],
) -> list[dict[str, Any]]:
    """Return leads shown false by an investigator with verified citations."""
    by_id = {lead["id"]: lead for lead in leads[:MAX_LEADS] if isinstance(lead.get("id"), int)}
    out: list[dict[str, Any]] = []
    seen_leads: set[int] = set()
    for lens, item in _collect_refutation_claims(outcomes):
        raw_id = item.get("lead")
        lead = by_id.get(raw_id) if type(raw_id) is int else None
        why = _text(item.get("why"), 300)
        if lead is None or not why or id(lead) in seen_leads:
            continue
        citations, problem, _beyond = check_citations(repo, item.get("evidence"), diff_lines)
        if problem is None:
            seen_leads.add(id(lead))
            out.append({"lead": lead, "lens": lens, "why": why, "citations": citations})
    return out


def _plural(count: int, singular: str) -> str:
    return singular if count == 1 else f"{singular}s"


def _assurance_tests_clause(test_count: int) -> str:
    if not test_count:
        return " (no test file mentions them)"
    return f" and {test_count} {_plural(test_count, 'test file')}"


def _assurance_claims_clause(claim_count: int) -> str:
    if not claim_count:
        return ""
    verb = "was" if claim_count == 1 else "were"
    noun = _plural(claim_count, "claim")
    return f", and {claim_count} {noun} of the change {verb} checked against the code"


def assurance(deep: dict[str, Any]) -> list[str]:
    """Two to four sentences on what the deep pass traced and what held."""
    brief: Brief = deep["brief"]
    ran = [record["lens"] for record in deep["investigators"] if record["status"] != "failed"]
    if not ran:
        return []
    names = ", ".join(f"`{name}`" for name in brief.symbols[:4]) or "the changed code"
    verb = "were" if len(brief.symbols) > 1 else "was"
    uses_text = f"{brief.callers} other {_plural(brief.callers, 'use')}"
    tests_clause = _assurance_tests_clause(len(brief.tests))
    claims_clause = _assurance_claims_clause(brief.claims)
    first = f"Beyond the diff, {names} {verb} followed to {uses_text}{tests_clause}{claims_clause}."
    return [first, *deep["checked"][:3]]
