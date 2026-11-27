"""Schema, normalization, validation, coverage, and predicate helpers for compiled autoreview."""
from __future__ import annotations

import json
import re
from typing import Any

import autoreview_deep as deep
from review_api import FileDiff, _clip, _text
from review_prompts import CHECK_CATALOGUE, TRIGGERED_SHAPES

LOOKUPS = (
    "read",
    "read_diff",
    "grep",
    "list",
    "definition",
    "references",
    "history",
    "blame_range",
    "pickaxe",
)
TEST_OPS = ("run_tests", "mutation_check")
ITERABLE = ("grep", "references", "list", "history", "blame_range", "pickaxe")
OPS = (*LOOKUPS, *TEST_OPS, "for_each", "ask", "assert", "finding")
LEVELS = ("critical", "high", "medium", "low", "nit")
OLD_LEVELS = {"blocker": "critical", "major": "high", "minor": "low"}
LEVEL_TO_OLD = {
    "critical": "blocker",
    "high": "major",
    "medium": "minor",
    "low": "minor",
    "nit": "nit",
}

MAX_PROGRAM_STEPS = 80
MAX_STEPS = 120
LARGE_DIFF_LINES = 100
LARGE_PROGRAM_STEPS = 120
LARGE_STEPS = 200
MAX_ASKS = 40
MAX_FINDINGS = 12
MAX_FOR_EACH_ITEMS = 20
DEFAULT_FOR_EACH_ITEMS = 8

MAX_TEMPLATE_STEPS = 8
MAX_QUESTION_CHARS = 1_500
ASK_CONTEXT_CHARS = 12_000
ASK_MATERIAL_CHARS = 4_000
MAX_ASK_MATERIAL = 4
RESULT_CHARS = 24_000
RECORD_CHARS = 1_200
EVIDENCE_CHARS = 1_200
MAX_HOLDS = 3
RETRIEVAL_CHARS = 24_000
RETRIEVAL_HITS = 12

PLAN_STYLES = ("cell", "frame")
DEFAULT_PLAN_STYLE = "cell"
DEFAULT_PLAN_CELLS = 10
MAX_PLAN_CELLS = 12

DEFAULT_VERIFY_CANDIDATES = 12
MAX_VERIFY_CANDIDATES = 40
MAX_EXTRA_STEPS = 3
CANDIDATE_READ_RADIUS = 12
CITATION_READ_RADIUS = 6
CLOSEST_TEST_RADIUS = 25
DEPLOY_GLOBS = (
    ".github/**",
    "**/*.yml",
    "**/*.yaml",
    "**/*.tf",
    "**/*.env*",
    "**/Dockerfile*",
    "**/*.toml",
)

DEFAULT_CELL_THINKING = "medium"
CELL_TRANSCRIPT_FULL = 2
CELL_TIMEOUT_S = 120
CELL_OUTPUT_CHARS = 8_000
CELL_TRANSCRIPT_CHARS = 40_000
CELL_VALUE_CHARS = 4_000
CELL_VALUE_ITEMS = 20
CAPPED = "(more not shown)"

_ID = re.compile(r"^[A-Za-z_][A-Za-z0-9_-]{0,39}$")
_PLACEHOLDER = re.compile(
    r"\{\{\s*([A-Za-z_][A-Za-z0-9_\-]*(?:\[\d+\])?(?:\.[A-Za-z_][A-Za-z0-9_\-]*)*)\s*\}\}"
)
_COUNT = re.compile(r"^count\s*(==|!=|>=|<=|>|<)\s*(\d+)$")
_WORD = re.compile(r"^(status|answer)\s*(==|!=)\s*([a-z_]+)$")
_CONTAINS = re.compile(r"^(not\s+)?contains\s+(.+)$", re.S)
_HIT = re.compile(r"^(.+?):(\d+): ?(.*)$")
_TITLE_COUNT = re.compile(r"-> (\d+) (?:matches|commits)|\((\d+) entries\)")

STATUSES = ("passed", "failed", "could_not_run")
ANSWERS = ("yes", "no", "unclear")

PROGRAM_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {
        "summary": {"type": "string"},
        "uncovered": {"type": "array", "items": {"type": "string"}},
        "steps": {
            "type": "array",
            "maxItems": LARGE_PROGRAM_STEPS,
            "items": {
                "type": "object",
                "properties": {"id": {"type": "string"}, "op": {"enum": list(OPS)}},
                "required": ["id", "op"],
            },
        },
    },
    "required": ["steps"],
}

ASK_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {
        "answer": {"enum": list(ANSWERS)},
        "quote": {"type": "string"},
        "why": {"type": "string"},
    },
    "required": ["answer", "quote"],
}

_REQUIRED_OP_ARGS: dict[str, tuple[str, ...]] = {
    "read": ("path",),
    "read_diff": ("path",),
    "grep": ("pattern",),
    "list": ("dir",),
    "definition": ("symbol",),
    "references": ("symbol",),
    "history": ("path",),
    "blame_range": ("path",),
    "pickaxe": ("string",),
    "run_tests": ("paths",),
    "mutation_check": ("path", "line", "replacement", "tests"),
}

_FINDING_FIELDS = (
    "file",
    "line",
    "level",
    "category",
    "claim",
    "why",
    "fix",
    "scenario",
    "unpinned",
    "consequence",
)

_EMPTY_ITEM_LINES = frozenset(
    {
        "0 matches",
        "no commits found",
        "no commit added or removed it",
        "no history available for these lines",
    }
)

_EFFECT = re.compile(
    r"execute\(|\bDELETE\b|\bINSERT\b|\bUPDATE\b|\bDROP\b|\brm\b|\bmv\b|unlink|"
    r"\.write\(|commit\(|\bsend|requests\.|fetch\(|subprocess|os\.system|shutil\.",
    re.I,
)
_GUARD = re.compile(
    r"validate|check|assert|verify|sanitiz|raise ValueError|throw new",
    re.I,
)
_ERROR_PATH = re.compile(
    r"\b(except|catch|raise|throw|finally|rollback|on_error|errexit)\b|set -e",
    re.I,
)
_RETRY = re.compile(r"\b(retry|retries|attempt|timeout|poll|backoff|sleep)\b", re.I)
_WORKFLOW = re.compile(
    r"(^|/)\.github/workflows/|(^|/)\.gitlab-ci|(^|/)ci/|Jenkinsfile|\.circleci/",
    re.I,
)
_MANIFEST = re.compile(
    r"(^|/)(k8s|helm|charts|deploy|infra|terraform|manifests?)/|\.tf$|docker-compose|"
    r"(^|/)Dockerfile|\.(ya?ml)$",
    re.I,
)
_MIGRATION = re.compile(r"\.sql$|(^|/)(migrations?|alembic)/.*\.(py|ts|js|rb|sql)$", re.I)


def as_list(value: Any) -> list[Any]:
    if isinstance(value, list):
        return value
    if isinstance(value, tuple):
        return list(value)
    return []


def as_dict(value: Any) -> dict[str, Any]:
    return value if isinstance(value, dict) else {}


def map_legacy_severity(level: str) -> str:
    return LEVEL_TO_OLD[level]


def map_legacy_level(name: str) -> str | None:
    return OLD_LEVELS.get(name)


def _flat(text: str) -> str:
    return " ".join(text.split())


def _parse_contains_predicate(match: re.Match[str]) -> tuple[str, ...] | None:
    needle = match.group(2).strip()
    if len(needle) >= 2 and needle[0] == needle[-1] and needle[0] in "\"'":
        needle = needle[1:-1]
    if not needle:
        return None
    negated = "not" if match.group(1) else ""
    return ("contains", needle, negated)


def _parse_predicate(text: Any) -> tuple[str, ...] | None:
    """Parse a predicate tuple: count, status/answer, or contains."""
    if not isinstance(text, str):
        return None
    cleaned = text.strip()
    if not cleaned:
        return None

    count_match = _COUNT.match(cleaned)
    if count_match:
        return ("count", count_match.group(1), count_match.group(2))

    word_match = _WORD.match(cleaned)
    if word_match:
        allowed = STATUSES if word_match.group(1) == "status" else ANSWERS
        if word_match.group(3) not in allowed:
            return None
        return (word_match.group(1), word_match.group(2), word_match.group(3))

    contains_match = _CONTAINS.match(cleaned)
    if contains_match:
        return _parse_contains_predicate(contains_match)
    return None


def _when(raw: Any) -> tuple[dict[str, Any] | None, str | None]:
    if raw is None:
        return None, None
    if isinstance(raw, str):
        raw = {"step": raw}
    if not isinstance(raw, dict) or not isinstance(raw.get("step"), str):
        return None, 'when must be {"step": <id>} or {"step": <id>, "not": true}'
    return {"step": raw["step"], "not": raw.get("not") is True}, None


def _ids(raw: Any) -> list[str]:
    if not isinstance(raw, list):
        return []
    return [item for item in raw if isinstance(item, str)]


def limits_for(changed_lines: int) -> tuple[int, int]:
    """Return (steps as written, steps after expansion) for `changed_lines`."""
    if changed_lines > LARGE_DIFF_LINES:
        return LARGE_PROGRAM_STEPS, LARGE_STEPS
    return MAX_PROGRAM_STEPS, MAX_STEPS


class Program:
    """A validated program with normalized steps and declared uncovered items."""

    def __init__(
        self,
        steps: list[dict[str, Any]],
        summary: str,
        uncovered: list[str] | None = None,
    ) -> None:
        self.steps = steps
        self.summary = summary
        self.uncovered = list(as_list(uncovered))

    def as_json(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "summary": self.summary,
            "steps": [_public_step(step) for step in self.steps],
        }
        if self.uncovered:
            out["uncovered"] = self.uncovered
        return out


def _public_step(step: dict[str, Any]) -> dict[str, Any]:
    out = {
        key: value
        for key, value in step.items()
        if key != "predicate_parsed" and value not in (None, [], {})
    }
    if "steps" in out:
        out["steps"] = [_public_step(item) for item in out["steps"]]
    return out


def _normalize_lookup_or_test(
    raw: dict[str, Any],
    step: dict[str, Any],
    label: str,
    errors: list[str],
) -> dict[str, Any] | None:
    op_name = step["op"]
    args = raw.get("args")
    if not isinstance(args, dict):
        errors.append(f"{label}: {op_name} takes an args object")
        return None

    for key in _REQUIRED_OP_ARGS[op_name]:
        if key not in args:
            errors.append(f"{label}: {op_name} needs args.{key}")
    step["args"] = args
    return step


def _parse_max_items(raw_limit: Any) -> int:
    try:
        parsed = int(raw_limit or DEFAULT_FOR_EACH_ITEMS)
    except (TypeError, ValueError):
        return DEFAULT_FOR_EACH_ITEMS
    return min(MAX_FOR_EACH_ITEMS, max(1, parsed))


def _normalize_for_each(
    raw: dict[str, Any],
    step: dict[str, Any],
    label: str,
    errors: list[str],
    inside: str | None,
) -> dict[str, Any] | None:
    if inside is not None:
        errors.append(f"{label}: for_each cannot be nested")
        return None

    if not isinstance(raw.get("over"), str):
        errors.append(
            f"{label}: for_each needs over "
            "(the id of a grep, references, list or history step)"
        )
    step["over"] = raw.get("over")
    step["max_items"] = _parse_max_items(raw.get("max_items"))

    template = raw.get("steps")
    if not isinstance(template, list) or not 1 <= len(template) <= MAX_TEMPLATE_STEPS:
        errors.append(f"{label}: for_each takes 1 to {MAX_TEMPLATE_STEPS} sub-steps")
        template = []

    normalized_subs: list[dict[str, Any]] = []
    for sub in template:
        item = _normalize_step(sub, errors, inside=step["id"])
        if item is not None:
            normalized_subs.append(item)
    step["steps"] = normalized_subs
    return step


def _normalize_ask(
    raw: dict[str, Any],
    step: dict[str, Any],
    label: str,
    errors: list[str],
) -> dict[str, Any]:
    question = raw.get("question")
    if not isinstance(question, str) or not question.strip():
        errors.append(f"{label}: ask needs a question")
    elif len(question) > MAX_QUESTION_CHARS:
        errors.append(f"{label}: the question is longer than {MAX_QUESTION_CHARS} characters")

    step["question"] = question if isinstance(question, str) else ""
    step["context"] = _ids(raw.get("context"))
    if not step["context"]:
        errors.append(
            f"{label}: ask needs context: the ids of the steps whose results answer it"
        )

    material_list = as_list(raw.get("material"))
    step["material"] = [
        _clip(item, ASK_MATERIAL_CHARS)
        for item in material_list
        if isinstance(item, str) and item.strip()
    ][:MAX_ASK_MATERIAL]
    return step


def _normalize_assert_step_form(
    raw: dict[str, Any],
    step: dict[str, Any],
    label: str,
    errors: list[str],
) -> None:
    if not isinstance(raw.get("step"), str):
        errors.append(f"{label}: assert.step must be an id")
    parsed = _parse_predicate(raw.get("predicate"))
    if parsed is None:
        errors.append(
            f"{label}: predicate {raw.get('predicate')!r} is not one of "
            "count ==|!=|>=|<=|>|< N, status == passed|failed|could_not_run, "
            "answer == yes|no|unclear, contains <text>, not contains <text>"
        )
    step["step"] = raw.get("step")
    predicate = raw.get("predicate")
    step["predicate"] = predicate if isinstance(predicate, str) else ""
    step["predicate_parsed"] = parsed


def _normalize_assert_groups(
    raw: dict[str, Any],
    step: dict[str, Any],
    label: str,
    errors: list[str],
) -> None:
    for key in ("all", "any"):
        if raw.get(key) is None:
            continue
        step[key] = _ids(raw.get(key))
        if not step[key]:
            errors.append(f"{label}: assert.{key} must list ids")


def _normalize_assert(
    raw: dict[str, Any],
    step: dict[str, Any],
    label: str,
    errors: list[str],
) -> dict[str, Any]:
    forms = [key for key in ("step", "all", "any") if raw.get(key) is not None]
    if len(forms) != 1:
        errors.append(
            f"{label}: assert takes exactly one of step (with predicate), all, any"
        )

    if raw.get("step") is not None:
        _normalize_assert_step_form(raw, step, label, errors)
    _normalize_assert_groups(raw, step, label, errors)

    expect = raw.get("expect")
    if not isinstance(expect, bool):
        errors.append(
            f"{label}: assert needs expect: true or false, "
            "the value you believe it will have"
        )
    step["expect"] = expect if isinstance(expect, bool) else None
    holds = raw.get("holds")
    step["holds"] = holds if isinstance(holds, str) else ""
    return step


def _valid_finding_line(line: Any) -> bool:
    if isinstance(line, int) and not isinstance(line, bool):
        return True
    if not isinstance(line, str):
        return False
    return line.strip().isdigit() or bool(_PLACEHOLDER.search(line))


def _check_finding_core(raw: dict[str, Any], label: str, errors: list[str]) -> None:
    for key in ("file", "claim"):
        value = raw.get(key)
        if not isinstance(value, str) or not value.strip():
            errors.append(f"{label}: finding needs {key}")

    if not _valid_finding_line(raw.get("line")):
        errors.append(f"{label}: finding needs line (a number or a placeholder)")

    level = str(raw.get("level", "")).strip().lower()
    if level not in LEVELS and map_legacy_level(level) is None:
        errors.append(f"{label}: level must be one of {', '.join(LEVELS)}")


def _normalize_finding(
    raw: dict[str, Any],
    step: dict[str, Any],
    label: str,
    errors: list[str],
) -> dict[str, Any]:
    when, _problem = _when(raw.get("when"))
    if when is None:
        errors.append(f"{label}: a finding needs when, naming an assert or an ask")
    step["when"] = when

    _check_finding_core(raw, label, errors)

    step["evidence"] = _ids(raw.get("evidence"))
    citations = as_list(raw.get("citations"))
    step["citations"] = [item for item in citations if isinstance(item, dict)]
    if not step["evidence"] and not step["citations"]:
        errors.append(f"{label}: finding needs evidence (step ids) or citations")

    for key in _FINDING_FIELDS:
        step[key] = raw.get(key)
    return step


def _normalize_step(
    raw: Any,
    errors: list[str],
    *,
    inside: str | None = None,
) -> dict[str, Any] | None:
    """Check and normalize one step; append any errors with the step's id."""
    if not isinstance(raw, dict):
        errors.append("a step is not an object")
        return None

    sid = raw.get("id")
    label = f"step {sid!r}" if isinstance(sid, str) else "a step without id"
    if not isinstance(sid, str) or not _ID.match(sid):
        errors.append(f"{label}: id must match {_ID.pattern}")
        return None

    op_name = raw.get("op")
    if op_name not in OPS:
        errors.append(f"{label}: unknown op {op_name!r}; use one of {', '.join(OPS)}")
        return None

    step: dict[str, Any] = {
        "id": sid,
        "op": op_name,
        "needs": _ids(raw.get("needs")),
        "covers": _ids(raw.get("covers")),
    }
    when, problem = _when(raw.get("when"))
    if problem:
        errors.append(f"{label}: {problem}")
    step["when"] = when

    if op_name in _REQUIRED_OP_ARGS:
        return _normalize_lookup_or_test(raw, step, label, errors)
    if op_name == "for_each":
        return _normalize_for_each(raw, step, label, errors, inside)
    normalizers = {
        "ask": _normalize_ask,
        "assert": _normalize_assert,
        "finding": _normalize_finding,
    }
    return normalizers[op_name](raw, step, label, errors)


def _references(step: dict[str, Any]) -> list[str]:
    """Every step id a step depends on (its needs and the ids its op reads)."""
    refs = list(as_list(step.get("needs")))
    if step.get("when"):
        refs.append(step["when"]["step"])
    for key in ("over", "step"):
        if isinstance(step.get(key), str):
            refs.append(step[key])
    for key in ("context", "all", "any", "evidence"):
        refs.extend(as_list(step.get(key)))
    return list(dict.fromkeys(refs))


def _assert_leaves(
    sid: str,
    by_id: dict[str, dict[str, Any]],
    seen: set[str] | None = None,
) -> list[dict[str, Any]]:
    """The non-assert steps an assert (or ask) ultimately reads, through all/any."""
    seen = seen if seen is not None else set()
    if sid in seen or sid not in by_id:
        return []
    seen.add(sid)
    step = by_id[sid]
    if step["op"] != "assert":
        return [step]

    refs: list[str] = []
    if isinstance(step.get("step"), str):
        refs.append(step["step"])
    refs.extend(as_list(step.get("all")))
    refs.extend(as_list(step.get("any")))

    out: list[dict[str, Any]] = []
    for ref in refs:
        out.extend(_assert_leaves(ref, by_id, seen))
    return out


def _is_exact_ground_leaf(leaf: dict[str, Any]) -> bool:
    if leaf["op"] in TEST_OPS:
        return True
    if leaf["op"] != "grep":
        return False
    args = leaf.get("args")
    return isinstance(args, dict) and args.get("count_only") is True


def grounded(step: dict[str, Any], by_id: dict[str, dict[str, Any]]) -> bool:
    """Whether a finding rests on an ask, a test run, or an exact-count presence check."""
    when = step.get("when")
    if when is None or when["step"] not in by_id:
        return True

    leaves = _assert_leaves(when["step"], by_id)
    if any(leaf["op"] == "ask" for leaf in leaves):
        return True

    ground_ops = (*TEST_OPS, "ask")
    evidence_refs = as_list(step.get("evidence"))
    if any(by_id.get(ref, {}).get("op") in ground_ops for ref in evidence_refs):
        return True

    return bool(leaves) and all(_is_exact_ground_leaf(leaf) for leaf in leaves)


def _diff_lines_by_kind(files: list[FileDiff], kind: str) -> str:
    return "\n".join(
        line.text
        for item in files
        for hunk in item.hunks
        for line in hunk.lines
        if line.kind == kind
    )


def _family_facts(brief: Any) -> list[dict[str, Any]]:
    reference = getattr(brief, "reference", None)
    facts_fn = getattr(reference, "facts", None)
    raw_facts = as_list(facts_fn() if callable(facts_fn) else None)
    return [fact for fact in raw_facts if fact.get("kind") == "family"]


def _registry_trigger(
    extracted: dict[str, Any],
    family_facts: list[dict[str, Any]],
    added: str,
) -> str | None:
    names = as_list(extracted.get("fields")) or as_list(extracted.get("constants"))
    has_literal = bool(re.search(r"^\s*['\"][\w.-]+['\"],?\s*$", added, re.M))
    if not names and not family_facts and not has_literal:
        return None
    if names:
        return ", ".join(f"`{name}`" for name in names[:4])
    if family_facts:
        return "; ".join(fact["id"] for fact in family_facts[:3])
    return "a new list member"


def _env_trigger(extracted: dict[str, Any]) -> str | None:
    combined = (
        as_list(extracted.get("env"))
        + as_list(extracted.get("flags"))
        + as_list(extracted.get("config"))[:4]
    )
    if not combined:
        return None
    return ", ".join(f"`{name}`" for name in combined)


def _comment_trigger(extracted: dict[str, Any], reviewable: list[FileDiff]) -> str | None:
    claims = as_list(extracted.get("claims"))
    has_doc = any(deep.file_kind(item.path) == "doc" for item in reviewable)
    if not claims and not has_doc:
        return None
    if claims:
        return "; ".join(_clip(text, 80) for _path, _line, text in claims[:3])
    return "the changed documents"


def _matching_paths(
    paths: list[str],
    pattern: re.Pattern[str],
    exclude: re.Pattern[str] | None = None,
) -> list[str]:
    matched = [path for path in paths if pattern.search(path)]
    if exclude is None:
        return matched
    return [path for path in matched if not exclude.search(path)]


def _detect_path_triggers(
    paths: list[str],
    reviewable: list[FileDiff],
    triggered: dict[str, str],
) -> None:
    workflows = _matching_paths(paths, _WORKFLOW)
    if workflows:
        triggered["workflow-siblings"] = ", ".join(workflows)[:200]

    manifests = _matching_paths(paths, _MANIFEST, exclude=_WORKFLOW)
    if manifests:
        triggered["manifest-reference"] = ", ".join(manifests)[:200]

    reviewable_paths = [item.path for item in reviewable]
    migrations = _matching_paths(reviewable_paths, _MIGRATION)
    if migrations:
        triggered["migration-safety"] = ", ".join(migrations[:3])


def _detect_guard_triggers(
    code: list[FileDiff],
    added: str,
    removed: str,
    triggered: dict[str, str],
) -> None:
    if not code:
        return
    if deep._INPUT_HINT.search(added):
        triggered["input-defeats-guard"] = "the new guard, pattern, limit or parser in the diff"
    if _GUARD.search(added) and _EFFECT.search(added + "\n" + removed):
        triggered["guard-after-effect"] = "the new check beside a side-effecting operation"
    if _RETRY.search(added):
        triggered["failure-retry"] = "the new retry, timeout or polling code"


def _detect_error_and_twin_triggers(
    added: str,
    removed: str,
    brief_text: str,
    triggered: dict[str, str],
) -> None:
    if _ERROR_PATH.search(added) or _ERROR_PATH.search(removed):
        triggered["error-path"] = "the changed except/catch/raise/rollback lines"
    if "same name elsewhere:" in brief_text:
        triggered["sibling-implementation"] = "the twin file(s) the brief names"


def _detect_file_kind_triggers(
    code: list[FileDiff],
    tests: list[FileDiff],
    triggered: dict[str, str],
) -> None:
    real_code = [item for item in code if deep.is_code(item.path)]
    if real_code:
        triggered["unpinned-behaviour"] = ", ".join(item.path for item in real_code[:3])
    if tests:
        triggered["test-asserts-behaviour"] = ", ".join(item.path for item in tests[:3])


def _detect_extracted_triggers(
    brief: Any,
    reviewable: list[FileDiff],
    added: str,
    triggered: dict[str, str],
) -> None:
    extracted = as_dict(getattr(brief, "extracted", None))
    registry = _registry_trigger(extracted, _family_facts(brief), added)
    if registry:
        triggered["registry-member"] = registry

    env_text = _env_trigger(extracted)
    if env_text:
        triggered["env-in-deploy"] = env_text

    comment_text = _comment_trigger(extracted, reviewable)
    if comment_text:
        triggered["comment-vs-code"] = comment_text


def _partition_reviewable(
    files: list[FileDiff],
) -> tuple[list[FileDiff], list[FileDiff], list[FileDiff]]:
    reviewable = [item for item in files if not deep.skip_reason(item)]
    code = [item for item in reviewable if deep.file_kind(item.path) == "code"]
    tests = [item for item in reviewable if deep.file_kind(item.path) == "test"]
    return reviewable, code, tests


def _format_shape_items(triggered: dict[str, str]) -> list[dict[str, str]]:
    by_key = {shape["key"]: shape for shape in CHECK_CATALOGUE}
    active_keys = [key for key in TRIGGERED_SHAPES if key in triggered]
    return [
        {
            "id": f"T{number}",
            "kind": "shape",
            "name": key,
            "text": (
                f"catalogue shape {key} ({triggered[key]}): "
                f"when {by_key[key]['when']}; how: {by_key[key]['how']}"
            ),
        }
        for number, key in enumerate(active_keys, 1)
    ]


def shape_items(brief: Any, files: list[FileDiff]) -> list[dict[str, str]]:
    """Catalogue shapes whose trigger the map detects in this change, as coverage items."""
    reviewable, code, tests = _partition_reviewable(files)
    added = _diff_lines_by_kind(reviewable, "+")
    removed = _diff_lines_by_kind(reviewable, "-")
    paths = [item.path for item in files]
    brief_text = getattr(brief, "text", "") or ""

    triggered: dict[str, str] = {}
    _detect_extracted_triggers(brief, reviewable, added, triggered)
    _detect_file_kind_triggers(code, tests, triggered)
    _detect_path_triggers(paths, reviewable, triggered)
    _detect_guard_triggers(code, added, removed, triggered)
    _detect_error_and_twin_triggers(added, removed, brief_text, triggered)
    return _format_shape_items(triggered)


def _symbol_coverage_items(siblings: list[str]) -> list[dict[str, str]]:
    return [
        {
            "id": f"S{number}",
            "kind": "symbol",
            "name": name,
            "text": (
                f"the references of `{name}` (changed signature or exported name): "
                "every caller still fits"
            ),
        }
        for number, name in enumerate(siblings, 1)
    ]


def _key_coverage_items(keys: list[str]) -> list[dict[str, str]]:
    return [
        {
            "id": f"K{number}",
            "kind": "key",
            "name": name,
            "text": (
                f"the siblings and consumers of the new key, field, flag or variable `{name}`: "
                "the family it joins (registry lists, sibling declarations) and every reader"
            ),
        }
        for number, name in enumerate(keys, 1)
    ]


def _claim_coverage_items(claims: list[tuple[str, str]]) -> list[dict[str, str]]:
    return [
        {
            "id": f"C{number}",
            "kind": "claim",
            "name": "",
            "text": f"the claim [{source}] {text}",
        }
        for number, (source, text) in enumerate(claims, 1)
    ]


def coverage_items(brief: Any, files: list[FileDiff] | None = None) -> list[dict[str, str]]:
    """What a program must cover: symbols (S), keys (K), claims (C), and shapes (T)."""
    required = as_dict(getattr(brief, "required", None))
    extracted = as_dict(getattr(brief, "extracted", None))
    siblings = as_list(required.get("siblings"))
    raw_keys = as_list(required.get("claims")) + as_list(extracted.get("fields"))
    keys = list(dict.fromkeys(raw_keys))[:8]
    claims = as_list(getattr(brief, "claim_list", None))[:8]

    items = _symbol_coverage_items(siblings)
    items.extend(_key_coverage_items(keys))
    items.extend(_claim_coverage_items(claims))
    if files is not None:
        items.extend(shape_items(brief, files))
    return items


def _step_texts(step: dict[str, Any]) -> str:
    """Text of a step where a coverage name may appear."""
    parts = [
        json.dumps(as_dict(step.get("args"))),
        step.get("question") or "",
        step.get("predicate") or "",
        step.get("holds") or "",
        str(step.get("claim") or ""),
        str(step.get("why") or ""),
    ]
    return "\n".join(part for part in parts if isinstance(part, str))


def uncovered_items(
    coverage: list[dict[str, str]],
    steps: list[dict[str, Any]],
    declared: list[str],
) -> list[dict[str, str]]:
    """The coverage items no step covers and the planner did not declare uncovered."""
    declared_ids = {item.split(":", 1)[0].strip() for item in declared}
    named: set[str] = set()
    texts: list[str] = []
    for step in steps:
        named.update(as_list(step.get("covers")))
        texts.append(_step_texts(step))
    text = "\n".join(texts)

    out: list[dict[str, str]] = []
    for item in coverage:
        if item["id"] in declared_ids or item["id"] in named:
            continue
        if item["kind"] in ("symbol", "key") and item["name"]:
            pattern = r"(?<![A-Za-z0-9_])" + re.escape(item["name"]) + r"(?![A-Za-z0-9_])"
            if re.search(pattern, text):
                continue
        out.append(item)
    return out


def _check_ask_refs(
    step: dict[str, Any],
    by_id: dict[str, dict[str, Any]],
    errors: list[str],
) -> None:
    allowed = LOOKUPS + TEST_OPS
    for ref in step["context"]:
        if ref in by_id and by_id[ref]["op"] not in allowed:
            errors.append(
                f"step {step['id']!r}: context must name lookups or test steps, "
                f"not {by_id[ref]['op']!r} {ref!r}"
            )


def _check_assert_refs(
    step: dict[str, Any],
    by_id: dict[str, dict[str, Any]],
    errors: list[str],
) -> None:
    target_ref = step.get("step")
    if isinstance(target_ref, str) and target_ref in by_id:
        target_op = by_id[target_ref]["op"]
        if target_op in ("assert", "finding", "for_each"):
            errors.append(
                f"step {step['id']!r}: a predicate applies to a lookup, test or ask, "
                f"not to {target_op!r} {target_ref!r}"
            )

    compound_refs = as_list(step.get("all")) + as_list(step.get("any"))
    for ref in compound_refs:
        if ref in by_id and by_id[ref]["op"] not in ("assert", "ask"):
            errors.append(
                f"step {step['id']!r}: all/any list asserts and asks, "
                f"not {by_id[ref]['op']!r} {ref!r}"
            )


def _check_finding_refs(
    step: dict[str, Any],
    by_id: dict[str, dict[str, Any]],
    errors: list[str],
) -> None:
    allowed = LOOKUPS + TEST_OPS + ("ask",)
    for ref in step["evidence"]:
        if ref in by_id and by_id[ref]["op"] not in allowed:
            errors.append(
                f"step {step['id']!r}: evidence names lookups, tests or asks, "
                f"not {by_id[ref]['op']!r} {ref!r}"
            )

    if not grounded(step, by_id):
        errors.append(
            f"step {step['id']!r}: a finding must rest on an ask (for code semantics), "
            'a test run, or an exact-count presence check (a grep with "count_only": true); '
            "a capped grep count or a read alone does not establish it"
        )


def _check_for_each_refs(
    step: dict[str, Any],
    by_id: dict[str, dict[str, Any]],
    errors: list[str],
) -> None:
    over = step.get("over")
    if over in by_id and by_id[over]["op"] not in ITERABLE:
        errors.append(
            f"step {step['id']!r}: for_each iterates a grep, references, list or history "
            f"result, not {by_id[over]['op']!r}"
        )


def _check_when_ref(
    step: dict[str, Any],
    by_id: dict[str, dict[str, Any]],
    errors: list[str],
) -> None:
    when = step.get("when")
    if not when or when["step"] not in by_id:
        return
    when_op = by_id[when["step"]]["op"]
    if when_op not in ("assert", "ask"):
        errors.append(
            f"step {step['id']!r}: when must name an assert or an ask, not {when_op!r}"
        )


def _check_step(
    step: dict[str, Any],
    by_id: dict[str, dict[str, Any]],
    scope: set[str],
    errors: list[str],
) -> None:
    """The reference rules of one step against the steps it may name."""
    for ref in _references(step):
        if ref not in by_id:
            errors.append(f"step {step['id']!r}: names unknown step {ref!r}")
        elif ref not in scope:
            errors.append(
                f"step {step['id']!r}: cannot name {ref!r}, a sub-step of another for_each"
            )

    _check_when_ref(step, by_id, errors)
    checkers = {
        "ask": _check_ask_refs,
        "assert": _check_assert_refs,
        "finding": _check_finding_refs,
        "for_each": _check_for_each_refs,
    }
    checker = checkers.get(step["op"])
    if checker is not None:
        checker(step, by_id, errors)


def _index_sub_steps(
    step: dict[str, Any],
    by_id: dict[str, dict[str, Any]],
    owner: dict[str, str],
    errors: list[str],
) -> None:
    for sub in as_list(step.get("steps")):
        sub_id = sub["id"]
        if sub_id in by_id:
            msg = f"step {sub_id!r}: duplicate id (sub-step ids must be unique in the program too)"
            errors.append(msg)
        by_id[sub_id] = sub
        owner[sub_id] = step["id"]


def _index_program_steps(
    steps: list[dict[str, Any]],
    errors: list[str],
) -> tuple[dict[str, dict[str, Any]], dict[str, str]]:
    by_id: dict[str, dict[str, Any]] = {}
    owner: dict[str, str] = {}
    for step in steps:
        if step["id"] in by_id:
            errors.append(f"step {step['id']!r}: duplicate id")
        by_id[step["id"]] = step
        _index_sub_steps(step, by_id, owner, errors)
    return by_id, owner


def _check_all_step_refs(
    steps: list[dict[str, Any]],
    by_id: dict[str, dict[str, Any]],
    errors: list[str],
) -> None:
    top = {step["id"] for step in steps}
    for step in steps:
        _check_step(step, by_id, top, errors)
        sub_steps = as_list(step.get("steps"))
        sub_scope = top | {other["id"] for other in sub_steps}
        for sub in sub_steps:
            _check_step(sub, by_id, sub_scope, errors)
            if sub["op"] == "for_each":
                errors.append(f"step {sub['id']!r}: for_each cannot be nested")


def _check_dependency_cycles(
    by_id: dict[str, dict[str, Any]],
    owner: dict[str, str],
    errors: list[str],
) -> None:
    graph = {
        sid: [ref for ref in _references(step) if ref in by_id]
        for sid, step in by_id.items()
    }
    for sid, parent in owner.items():
        graph[parent].append(sid)

    state: dict[str, int] = {}

    def visit(node: str, path: list[str]) -> None:
        if state.get(node) == 2:
            return
        if state.get(node) == 1:
            cycle = " -> ".join(path[path.index(node) :] + [node])
            errors.append("the dependencies form a cycle: " + cycle)
            return
        state[node] = 1
        for ref in graph.get(node, []):
            visit(ref, path + [node])
        state[node] = 2

    for sid in by_id:
        visit(sid, [])


def _check_coverage_rules(
    steps: list[dict[str, Any]],
    coverage: list[dict[str, str]],
    declared: list[str],
    errors: list[str],
) -> None:
    all_steps = list(steps) + [sub for step in steps for sub in as_list(step.get("steps"))]
    for item in uncovered_items(coverage, all_steps, declared):
        errors.append(
            f"coverage: {item['id']} ({item['text']}) has no check: add a step that checks it "
            f'and name {item["id"]} in its "covers", or list it in the program\'s "uncovered" '
            "with why"
        )

    known = {item["id"] for item in coverage}
    for item in declared:
        cid = item.split(":", 1)[0].strip()
        if cid not in known:
            errors.append(f"uncovered names {cid!r}, which is not a coverage item")


def _parse_declared_uncovered(raw: dict[str, Any]) -> list[str]:
    uncovered = raw.get("uncovered")
    if not isinstance(uncovered, list):
        return []
    return [_text(item, 200) for item in uncovered if isinstance(item, str)]


def _check_program_counts(by_id: dict[str, dict[str, Any]], errors: list[str]) -> None:
    asks = sum(1 for step in by_id.values() if step["op"] == "ask")
    if asks > MAX_ASKS:
        errors.append(f"{asks} ask steps; at most {MAX_ASKS}")

    findings = sum(1 for step in by_id.values() if step["op"] == "finding")
    if findings > MAX_FINDINGS:
        errors.append(f"{findings} finding steps; at most {MAX_FINDINGS}")


def _validate_top_shape(raw: Any, max_planned: int) -> str | None:
    if not isinstance(raw, dict) or not isinstance(raw.get("steps"), list):
        return "the program must be an object with a steps array"
    if not raw["steps"]:
        return "the program has no steps"
    if len(raw["steps"]) > max_planned:
        return f"the program has {len(raw['steps'])} steps; at most {max_planned} as written"
    return None


def validate(
    raw: Any,
    coverage: list[dict[str, str]] | None = None,
    max_planned: int = MAX_PROGRAM_STEPS,
) -> tuple[Program | None, list[str]]:
    """Validate a raw program against the language and coverage rules."""
    top_error = _validate_top_shape(raw, max_planned)
    if top_error is not None:
        return None, [top_error]

    errors: list[str] = []
    declared = _parse_declared_uncovered(raw)
    steps: list[dict[str, Any]] = []
    for entry in raw["steps"]:
        normalized = _normalize_step(entry, errors)
        if normalized is not None:
            steps.append(normalized)

    by_id, owner = _index_program_steps(steps, errors)
    _check_all_step_refs(steps, by_id, errors)
    _check_dependency_cycles(by_id, owner, errors)
    _check_program_counts(by_id, errors)

    if coverage:
        _check_coverage_rules(steps, coverage, declared, errors)

    if errors:
        return None, list(dict.fromkeys(errors))[:40]
    return Program(steps, _text(raw.get("summary"), 300), declared), []


def _count_of(op: str, title: str, body: str) -> int | None:
    match = _TITLE_COUNT.search(title)
    if match:
        return int(match.group(1) or match.group(2))
    if op in ("read", "read_diff"):
        return len(body.splitlines())
    return None


def _parse_hit_items(body: str) -> list[dict[str, Any]]:
    items: list[dict[str, Any]] = []
    for line in body.splitlines():
        match = _HIT.match(line)
        if not match:
            continue
        items.append({"path": match.group(1), "line": int(match.group(2)), "text": match.group(3)})
    return items


def _items_of(op: str, body: str) -> list[Any]:
    if op in ("grep", "references", "definition"):
        return _parse_hit_items(body)
    if op in ("list", "history", "blame_range", "pickaxe"):
        return [
            line
            for line in body.splitlines()
            if line.strip() and not line.startswith("...") and line not in _EMPTY_ITEM_LINES
        ]
    return []


def _render(value: Any) -> str:
    if isinstance(value, dict):
        if {"path", "line"} <= set(value):
            return f"{value['path']}:{value['line']}"
        return json.dumps(value)
    return "" if value is None else str(value)


def _exact_count_compare(count: int, op: str, target: int) -> bool:
    comparisons = {
        "==": count == target,
        "!=": count != target,
        ">=": count >= target,
        "<=": count <= target,
        ">": count > target,
        "<": count < target,
    }
    return comparisons[op]


def _capped_count_compare(count: int, op: str, target: int) -> bool | None:
    settled_map = {
        "==": False if target < count else None,
        "!=": True if target < count else None,
        ">=": True if count >= target else None,
        ">": True if count > target else None,
        "<": False if count >= target else None,
        "<=": False if count > target else None,
    }
    return settled_map.get(op)


def _count_predicate(count: int, capped: bool, op: str, n: int) -> tuple[bool | None, str]:
    """A count predicate over an exact count, or over a lower bound (cut at its cap)."""
    if not capped:
        return _exact_count_compare(count, op, n), f"count is {count}"
    return _capped_count_compare(count, op, n), f"count is at least {count} (cut at its cap)"
