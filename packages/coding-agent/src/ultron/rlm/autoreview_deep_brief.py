"""Diff symbol extraction and investigation brief construction for `autoreview_deep`."""
from __future__ import annotations

import posixpath
import re
from dataclasses import dataclass, field
from typing import Any, Callable

import autoreview_reference as reference_module
from autoreview_deep_repo import (
    BRIEF_CHARS,
    MAX_LIKE,
    MAX_LIKE_SCANNED,
    Repo,
    _add,
    _definition_pattern,
    _numbered,
    is_code,
)
from knowledge_api import construct_label
from review_api import (
    _CALL,
    _COMMON,
    _DEF,
    _TEST,
    FileDiff,
    _clip,
    _text,
    file_kind,
    skip_reason,
)

_CONSTANT = re.compile(
    r"^\s*(?:export\s+)?(?:const\s+|final\s+|static\s+){0,3}([A-Z][A-Z0-9_]{2,})\s*[:=]"
)
_ENV = re.compile(
    r"""(?:environ(?:\.get)?\s*[\[(]\s*|getenv\(\s*|process\.env\.|\$\{?)["']?([A-Z][A-Z0-9_]{3,})"""
)
_FLAG = re.compile(r"""["'\s](--[a-z][a-z0-9-]{2,})""")
_TABLE = re.compile(
    r"\b(?:alter|create)\s+table\s+(?:if\s+(?:not\s+)?exists\s+)?[`\"\[]?(\w+)"
    r"|\badd\s+column\s+(?:if\s+not\s+exists\s+)?[`\"\[]?(\w+)",
    re.I,
)
_COMMENT = re.compile(r"^\s*(?:#+|//+|/?\*+|--|<!--|\"\"\"|''')\s?(.{15,})$")
#: A changed code line that is behaviour: not blank, comment, or bare docstring/string.
_BEHAVIOUR = re.compile(
    r'^\s*(?!#|//|/\*|\*|--|<!--|-->)(?!"""[^"]*("""|$))'
    r"(?!'''[^']*('''|$))(?!['\"][^'\"]*['\"]\s*,?\s*$)\S"
)
_DESCRIPTION = re.compile(r"""\b(?:description|help|doc|summary)\s*[:=]\s*["'](.{15,})["']""")
_DOCLIKE = re.compile(
    r"\.(md|mdx|rst|txt|adoc|ya?ml|toml|cfg|ini|json|sh|tf|env)$"
    r"|(^|/)(\.github|ci|scripts|docs?)/|(^|/)Dockerfile",
    re.I,
)
_STRUCTURE = r"parametrize|fixture|it\.each|test\.each|describe\(|@pytest\.mark"
_CONFIG_PATH = re.compile(
    r"\.(ya?ml|toml|cfg|ini|tf|env|properties)$|\.sql$"
    r"|(^|/)(\.github|ci|deploy|k8s|helm|charts|infra|terraform|migrations?|alembic)/"
    r"|(^|/)(Dockerfile|docker-compose[^/]*|Makefile|package\.json|requirements[^/]*\.txt"
    r"|pyproject\.toml|go\.mod|Cargo\.toml|Gemfile|pom\.xml|build\.gradle[^/]*)$",
    re.I,
)
_INPUT_HINT = re.compile(
    r"\bre\.|regex|RegExp|\.match\(|\.test\(|\.search\(|parse|split\(|strip\(|startswith|endswith"
    r"|validate|sanitiz|escape|limit|max_|min_|len\(|\.length|truncate|slice\(|\bint\(|float\("
    r"|isinstance|typeof|\bif .*(<|>|<=|>=|==|!=)",
    re.I,
)
_REQUEST_INPUT = re.compile(
    r"""\b(?:request|req|ctx)\.(?:args|form|json|GET|POST|params|query|body|headers|data)"""
    r"""(?:\.get\(\s*|\[\s*|\.)["']?([A-Za-z_][A-Za-z0-9_]{2,})"""
)
_CONFIG_KEY = re.compile(r"""^\s*-?\s*["']?([A-Za-z_][A-Za-z0-9_.-]{2,})["']?\s*[:=]""")
#: A field or key declared in code whose siblings and consumers must be checked.
_FIELD = re.compile(
    r"""^\s*\{?\s*(?:key|name|field|flag|option)\s*[:=]\s*["']([A-Za-z_][A-Za-z0-9_.-]{2,})["']"""
)
_CONFIG_FILE = re.compile(r"\.(ya?ml|toml|ini|cfg|env|properties|json|tf)$|(^|/)\.env", re.I)
_WORD = re.compile(r"`([^`]{2,60})`|\b([A-Za-z_][A-Za-z0-9_]{3,})\b")
_RISK = re.compile(
    r"auth|token|password|passwd|secret|credential|permission|role|session|cookie|sql|query\(|execute\("
    r"|eval\(|exec\(|subprocess|os\.system|shell|pickle|yaml\.load|request\.|route|endpoint|router\."
    r"|@app\.|email|phone|ssn|pii|encrypt|decrypt|hash|cors|csrf|upload|redirect",
    re.I,
)


@dataclass
class Brief:
    text: str
    lenses: list[str]
    symbols: list[str] = field(default_factory=list)
    callers: int = 0
    tests: list[str] = field(default_factory=list)
    claims: int = 0
    #: Names an investigator must look up before finishing, keyed by lens.
    required: dict[str, list[str]] = field(default_factory=dict)
    #: Claims listed in the brief as (source, text) pairs.
    claim_list: list[tuple[str, str]] = field(default_factory=list)
    helpers: list[str] = field(default_factory=list)
    extracted: dict[str, list[Any]] = field(default_factory=dict)
    #: Structural reference, its rendered block, and build stats.
    reference: Any = None
    reference_text: str = ""
    reference_stats: dict[str, Any] = field(default_factory=dict)


def _find_enclosing_def(source: list[str], first: int) -> str | None:
    start_idx = min(first, len(source)) - 1
    stop_idx = max(-1, first - 80)
    for index in range(start_idx, stop_idx, -1):
        match = _DEF.match(source[index])
        if match:
            return match.group(1) or match.group(2)
    return None


def _record_enclosing_defs(
    source: list[str], hunk: Any, added: list[Any], out: dict[str, list[Any]]
) -> None:
    for line in added:
        first = line.new or hunk.new_first
        name = _find_enclosing_def(source, first)
        if name:
            _add(out["changed"], name, 12)


def _extract_doc_line(path: str, line: Any, out: dict[str, list[Any]]) -> None:
    text = line.text
    if len(text.strip()) >= 20:
        _add(out["claims"], (path, line.new, _clip(text.strip(), 200)), 14)
    for flag in _FLAG.findall(" " + text):
        _add(out["flags"], flag, 6)


def _extract_code_line_patterns(
    path: str, line: Any, out: dict[str, list[Any]]
) -> None:
    text = line.text
    for name in _CALL.findall(text):
        if name not in _COMMON:
            _add(out["calls"], name, 16)
    for name in _ENV.findall(text):
        _add(out["env"], name, 6)
    for name in _REQUEST_INPUT.findall(text):
        _add(out["inputs"], name, 6)
    for flag in _FLAG.findall(" " + text):
        _add(out["flags"], flag, 6)
    for pair in _TABLE.findall(text):
        _add(out["tables"], pair[0] or pair[1], 6)
    claim = _COMMENT.match(text) or _DESCRIPTION.search(text)
    if claim:
        _add(out["claims"], (path, line.new, _clip(claim.group(1).strip(), 200)), 14)


def _extract_code_line(
    path: str, line: Any, is_test: bool, out: dict[str, list[Any]]
) -> None:
    text = line.text
    if not is_test:
        match = _DEF.match(text)
        if match:
            _add(out["defined"], match.group(1) or match.group(2), 12)
    const_match = _CONSTANT.match(text)
    if const_match:
        _add(out["constants"], const_match.group(1), 10)
    field_match = _FIELD.match(text)
    if field_match:
        _add(out["fields"], field_match.group(1), 8)
    _extract_code_line_patterns(path, line, out)


def _extract_added_line(
    path: str, line: Any, is_doc: bool, is_test: bool, out: dict[str, list[Any]]
) -> None:
    text = line.text
    if _CONFIG_FILE.search(path):
        key = _CONFIG_KEY.match(text)
        if key and not text.lstrip().startswith(("#", "//")):
            _add(out["config"], key.group(1), 8)
    if is_doc:
        _extract_doc_line(path, line, out)
        return
    _extract_code_line(path, line, is_test, out)


def _extract_file(
    item: FileDiff,
    read_file: Callable[[str], list[str] | None],
    out: dict[str, list[Any]],
) -> None:
    kind = file_kind(item.path)
    is_doc = kind == "doc"
    is_test = kind == "test"
    source = read_file(item.path)
    for hunk in item.hunks:
        added = [line for line in hunk.lines if line.kind == "+"]
        # The definition an added line sits in changed, even when its `def` line did not.
        if source and not is_doc and not is_test:
            _record_enclosing_defs(source, hunk, added, out)
        for line in added:
            _extract_added_line(item.path, line, is_doc, is_test, out)


def extract(
    files: list[FileDiff], read_file: Callable[[str], list[str] | None]
) -> dict[str, list[Any]]:
    """What the diff defines, changes, calls and claims: names worth following into the repository."""
    out: dict[str, list[Any]] = {
        "defined": [],
        "changed": [],
        "calls": [],
        "constants": [],
        "env": [],
        "flags": [],
        "tables": [],
        "claims": [],
        "inputs": [],
        "config": [],
        "fields": [],
    }
    for item in files:
        if skip_reason(item):
            continue
        _extract_file(item, read_file, out)
    own = set(out["defined"]) | set(out["changed"])
    out["calls"] = [name for name in out["calls"] if name not in own][:8]
    return out


def _build_reference(
    repo: Repo, files: list[FileDiff], base: str | None
) -> tuple[Any, str]:
    try:
        base_repo = Repo(repo.root, base, repo._runner) if base else None
        reference = reference_module.Reference(repo, files, base_repo=base_repo).build()
        return reference, ""
    except (OSError, RuntimeError, ValueError, TypeError, KeyError, AttributeError) as error:
        repo.reference = None
        return None, f"{type(error).__name__}: {error}"


def _symbol_hits(
    repo: Repo,
    name: str,
    changed_lines: dict[str, set[int]],
    reference: Any,
    per: int,
) -> tuple[list[tuple[str, int, str]], str]:
    hits = [
        hit
        for hit in repo.grep(name, fixed=True, word=True, limit=per + 12)
        if hit[1] not in changed_lines.get(hit[0], ())
    ]
    if reference is None or not reference.symbols_named(name):
        return hits, ""
    # Call sites resolved through imports first; text hits only from files the reference did not read.
    calls = [(call.path, call.line, call.text) for call in reference.callers(name)]
    if not calls:
        return hits, ""
    merged = calls + [hit for hit in hits if hit[0] not in reference.index]
    return merged, " (callers resolved)"


def _format_uses_section(
    title: str,
    name: str,
    resolved: str,
    others: list[tuple[str, int, str]],
    in_tests: list[tuple[str, int, str]],
    in_docs: list[tuple[str, int, str]],
    per: int,
) -> str:
    used_text = "; ".join(
        f"{path}:{line}: {_clip(text.strip(), 110)}" for path, line, text in others[:per]
    )
    tests_text = "; ".join(
        f"{path}:{line}: {_clip(text.strip(), 110)}" for path, line, text in in_tests[:5]
    )
    lines = [
        f"{title} `{name}`{resolved}:",
        f"  used at: {used_text or 'nowhere else'}",
        f"  tests: {tests_text or 'no test mentions it'}",
    ]
    if in_docs:
        docs_text = "; ".join(f"{path}:{line}" for path, line, _ in in_docs[:6])
        lines.append(f"  docs and configs: {docs_text}")
    return "\n".join(lines)


def _classify_hits(
    hits: list[tuple[str, int, str]],
) -> tuple[list[tuple[str, int, str]], list[tuple[str, int, str]], list[tuple[str, int, str]]]:
    in_tests: list[tuple[str, int, str]] = []
    in_docs: list[tuple[str, int, str]] = []
    others: list[tuple[str, int, str]] = []
    for hit in hits:
        if _TEST.search(hit[0]):
            in_tests.append(hit)
        elif _DOCLIKE.search(hit[0]):
            in_docs.append(hit)
        else:
            others.append(hit)
    return others, in_tests, in_docs


def _collect_uses(
    repo: Repo,
    names: list[str],
    title: str,
    changed_lines: dict[str, set[int]],
    reference: Any,
    used_at: dict[str, list[str]],
    tests: list[str],
    per: int = 8,
) -> tuple[list[str], int]:
    sections: list[str] = []
    callers = 0
    for name in names:
        hits, resolved = _symbol_hits(repo, name, changed_lines, reference, per)
        others, in_tests, in_docs = _classify_hits(hits)
        callers += len(others)
        used_at[name] = [f"{path}:{line}" for path, line, _ in (others + in_tests)[:5]]
        for hit in in_tests:
            _add(tests, hit[0], 6)
        sections.append(
            _format_uses_section(title, name, resolved, others, in_tests, in_docs, per)
        )
    return sections, callers


def _collect_all_uses(
    repo: Repo,
    found: dict[str, list[Any]],
    symbols: list[str],
    changed_lines: dict[str, set[int]],
    reference: Any,
    used_at: dict[str, list[str]],
    tests: list[str],
) -> tuple[list[str], int]:
    named_items = (found["env"] + found["flags"] + found["tables"])[:6]
    groups = (
        (symbols, "Changed or added"),
        (found["constants"][:5], "Constant or member"),
        (named_items, "Named in the change"),
    )
    sections: list[str] = []
    callers = 0
    for names, heading in groups:
        part_sections, part_callers = _collect_uses(
            repo, names, heading, changed_lines, reference, used_at, tests
        )
        sections.extend(part_sections)
        callers += part_callers
    return sections, callers


def _collect_helper_sections(
    repo: Repo, calls: list[str]
) -> tuple[list[str], list[str]]:
    helpers: list[str] = []
    sections: list[str] = []
    for name in calls[:5]:
        hits = repo.grep(_definition_pattern(name), limit=3)
        if not hits:
            continue
        path, line, _ = hits[0]
        lines = repo.lines(path)
        if not lines:
            continue
        helpers.append(name)
        snippet = _numbered(lines, line, min(len(lines), line + 11))
        sections.append(f"Called by the change, `{name}` is defined at {path}:{line}:\n{snippet}")
    return helpers, sections


def _collect_test_structures(repo: Repo, test_paths: list[str]) -> list[str]:
    sections: list[str] = []
    for path in list(dict.fromkeys(test_paths))[:4]:
        hits = repo.grep(_STRUCTURE, pathspec=path, limit=8)
        if hits:
            summary = "; ".join(f"{line}: {_clip(text.strip(), 120)}" for _, line, text in hits)
            sections.append(f"Structure of {path}: {summary}")
    return sections


def _collect_like_sections(
    code_files: list[str], changed_paths: set[str], knowledge: Any
) -> tuple[list[str], set[str]]:
    if knowledge is None:
        return [], set()
    like_sections: list[str] = []
    alike_for: set[str] = set()
    for path in code_files[:MAX_LIKE_SCANNED]:
        alike = knowledge.similar(path, exclude=changed_paths)
        if not alike:
            continue
        alike_for.add(path)
        peers = "; ".join(
            f"{item.path} ({', '.join(construct_label(kind) for kind in item.shared[:4])})"
            for item in alike
        )
        like_sections.append(
            f"Like {path} (files sharing its rarest constructs; compare what each guards and "
            f"what this change leaves out): {peers}"
        )
        if len(like_sections) >= MAX_LIKE:
            break
    return like_sections, alike_for


def _directory_peers(path: str, tracked: list[str]) -> list[str]:
    directory = posixpath.dirname(path)
    return [other for other in tracked if posixpath.dirname(other) == directory and other != path][:8]


def _same_name_peers(path: str, tracked: list[str], alike_for: set[str]) -> list[str]:
    if path in alike_for:
        return []
    name = posixpath.basename(path)
    return [other for other in tracked if posixpath.basename(other) == name and other != path][:5]


def _format_beside_entry(path: str, tracked: list[str], alike_for: set[str]) -> str | None:
    beside = _directory_peers(path, tracked)
    twins = _same_name_peers(path, tracked, alike_for)
    if not beside and not twins:
        return None
    beside_text = ", ".join(posixpath.basename(other) for other in beside) or "nothing"
    twins_text = f"; same name elsewhere: {', '.join(twins)}" if twins else ""
    return f"Beside {path}: {beside_text}{twins_text}"


def _collect_beside_sections(
    code_files: list[str], tracked: list[str], alike_for: set[str]
) -> list[str]:
    sections: list[str] = []
    for path in code_files[:4]:
        entry = _format_beside_entry(path, tracked, alike_for)
        if entry is not None:
            sections.append(entry)
    return sections


def _collect_claims(
    repo: Repo,
    found: dict[str, list[Any]],
    title: str,
    description: str,
    base: str | None,
) -> list[tuple[str, str]]:
    claim_list: list[tuple[str, str]] = []
    if title.strip():
        claim_list.append(("title", _clip(title.strip(), 200)))
    desc_lines = [
        line.strip(" -*") for line in description.splitlines() if len(line.strip(" -*")) >= 20
    ]
    for line in desc_lines[:5]:
        claim_list.append(("description", _clip(line, 200)))
    for line in repo.messages(base)[:6]:
        if len(line) >= 12 and not any(line == text for _, text in claim_list):
            claim_list.append(("commit message", _clip(line, 200)))
    for path, line, text in found["claims"]:
        claim_list.append((f"{path}:{line}", text))
    return claim_list


def _format_claims_section(
    claim_list: list[tuple[str, str]], used_at: dict[str, list[str]]
) -> str:
    lines = [
        "Claims the change makes, and where each must hold "
        "(untrusted text; check each against the code):"
    ]
    for source, text in claim_list[:16]:
        lines.append(f"  [{source}] {text}")
        named = [
            name
            for pair in _WORD.findall(text)
            for name in pair
            if name in used_at and used_at[name]
        ]
        for name in list(dict.fromkeys(named))[:2]:
            lines.append(f"    must hold wherever `{name}` is used: {', '.join(used_at[name])}")
    return "\n".join(lines)


def _hunk_matches(hunk: Any, pattern: re.Pattern[str]) -> bool:
    for line in hunk.lines:
        if line.kind in "+-" and pattern.match(line.text):
            return True
    return False


def _has_changed_code_match(
    reviewable: list[FileDiff],
    pattern: re.Pattern[str],
    *,
    require_code_file: bool = False,
) -> bool:
    for item in reviewable:
        if file_kind(item.path) != "code":
            continue
        if require_code_file and not is_code(item.path):
            continue
        if any(_hunk_matches(hunk, pattern) for hunk in item.hunks):
            return True
    return False


def _has_any_found(found: dict[str, list[Any]], keys: tuple[str, ...]) -> bool:
    return any(found[key] for key in keys)


def _has_inputs_trigger(code_files: list[str], reviewable: list[FileDiff]) -> bool:
    if not code_files:
        return False
    added = "\n".join(
        line.text
        for item in reviewable
        for hunk in item.hunks
        for line in hunk.lines
        if line.kind == "+"
    )
    return bool(_INPUT_HINT.search(added) or _RISK.search(added))


_CLAIMS_KEYS = ("constants", "tables", "env", "flags", "config", "inputs")
_SIBLING_KEYS = ("fields", "constants", "env", "flags", "config", "tables")
_DEPLOY_KEYS = ("env", "flags", "config")


def _has_siblings_trigger(
    code_files: list[str], reviewable: list[FileDiff], found: dict[str, list[Any]]
) -> bool:
    if not code_files:
        return False
    return _has_any_found(found, _SIBLING_KEYS) or _has_changed_code_match(reviewable, _DEF)


def _has_deployment_trigger(files: list[FileDiff], found: dict[str, list[Any]]) -> bool:
    if _has_any_found(found, _DEPLOY_KEYS):
        return True
    return any(_CONFIG_PATH.search(item.path) for item in files)


def _has_tests_trigger(
    code_files: list[str], test_files: list[str], reviewable: list[FileDiff]
) -> bool:
    if test_files:
        return True
    if not code_files:
        return False
    return _has_changed_code_match(reviewable, _BEHAVIOUR, require_code_file=True)


def _select_lenses(
    files: list[FileDiff],
    reviewable: list[FileDiff],
    code_files: list[str],
    test_files: list[str],
    found: dict[str, list[Any]],
    symbols: list[str],
    claim_list: list[tuple[str, str]],
) -> list[str]:
    triggers = (
        ("claims", bool(claim_list or symbols or _has_any_found(found, _CLAIMS_KEYS))),
        ("siblings", _has_siblings_trigger(code_files, reviewable, found)),
        ("deployment", _has_deployment_trigger(files, found)),
        ("tests", _has_tests_trigger(code_files, test_files, reviewable)),
        ("inputs", _has_inputs_trigger(code_files, reviewable)),
    )
    return [name for name, active in triggers if active]


def _attach_reference(brief: Brief, reference: Any, reference_error: str) -> None:
    if reference is not None:
        try:
            brief.reference = reference
            brief.reference_text = reference.block()
            brief.reference_stats = dict(reference.stats)
        except Exception as error:
            brief.reference_text = ""
            reference_error = f"{type(error).__name__}: {error}"
    if reference_error:
        brief.reference_stats = {"error": _text(reference_error, 200)}


def _changed_lines_map(files: list[FileDiff]) -> dict[str, set[int]]:
    return {
        item.path: {
            line.new
            for hunk in item.hunks
            for line in hunk.lines
            if line.kind == "+" and line.new is not None
        }
        for item in files
    }


def _required_sections(required: dict[str, list[str]]) -> list[str]:
    sections: list[str] = []
    if required["claims"]:
        names = ", ".join(f"`{name}`" for name in required["claims"])
        sections.append(
            "New configuration and inputs, each to be traced from where it enters "
            f"to where it is read: {names}"
        )
    if required["siblings"]:
        names = ", ".join(f"`{name}`" for name in required["siblings"])
        sections.append(f"Changed signatures and exported names, each caller to be checked: {names}")
    return sections


def _assemble_brief_text(
    sections: list[str],
    claim_list: list[tuple[str, str]],
    used_at: dict[str, list[str]],
    like_sections: list[str],
    limit: int,
) -> str:
    ordered = list(sections)
    if claim_list:
        ordered.insert(0, _format_claims_section(claim_list, used_at))
    insert_at = 1 if claim_list else 0
    ordered[insert_at:insert_at] = like_sections

    text = "\n\n".join(ordered)
    if len(text) > limit:
        text = text[: limit - 40] + "\n... (brief cut at its size limit)"
    return text or "(nothing in the repository mentions the changed names)"


def build_brief(
    repo: Repo,
    files: list[FileDiff],
    read_file: Callable[[str], list[str] | None],
    limit: int = BRIEF_CHARS,
    *,
    title: str = "",
    description: str = "",
    base: str | None = None,
    knowledge: Any = None,
) -> Brief:
    """Build the investigation brief: claims, obligations, and repository references.
    With `knowledge`, each changed file also lists files sharing its rarest constructs."""
    found = extract(files, read_file)
    reference, reference_error = _build_reference(repo, files, base)
    changed_lines = _changed_lines_map(files)

    reviewable = [item for item in files if not skip_reason(item)]
    code_files = [item.path for item in reviewable if file_kind(item.path) == "code"]
    test_files = [item.path for item in reviewable if file_kind(item.path) == "test"]
    tests: list[str] = []
    used_at: dict[str, list[str]] = {}

    symbols = list(dict.fromkeys(found["defined"] + found["changed"]))[:8]
    sections, callers = _collect_all_uses(
        repo, found, symbols, changed_lines, reference, used_at, tests
    )
    helpers, helper_sections = _collect_helper_sections(repo, found["calls"])
    sections.extend(helper_sections)
    sections.extend(_collect_test_structures(repo, tests + test_files))

    changed_paths = {item.path for item in files}
    like_sections, alike_for = _collect_like_sections(code_files, changed_paths, knowledge)
    sections.extend(_collect_beside_sections(code_files, repo.files(), alike_for))

    claim_list = _collect_claims(repo, found, title, description, base)
    required = {
        "siblings": list(dict.fromkeys(found["defined"] + found["changed"]))[:6],
        "claims": list(
            dict.fromkeys(found["env"] + found["flags"] + found["config"] + found["inputs"])
        )[:6],
    }
    sections.extend(_required_sections(required))

    text = _assemble_brief_text(sections, claim_list, used_at, like_sections, limit)
    lenses = _select_lenses(files, reviewable, code_files, test_files, found, symbols, claim_list)
    brief = Brief(
        text,
        lenses,
        symbols,
        callers,
        tests,
        len(claim_list),
        required,
        claim_list,
        helpers,
        found,
    )
    _attach_reference(brief, reference, reference_error)
    return brief
