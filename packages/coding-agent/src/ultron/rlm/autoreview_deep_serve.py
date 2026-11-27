"""Request serving for `autoreview_deep`: read-only lookups, knowledge queries, and tests."""
from __future__ import annotations

import json
import posixpath
import re
from typing import Any, Callable

import autoreview_reference as reference_module
import autoreview_tests as testing
from autoreview_deep_repo import (
    DEFAULT_GREP_HITS,
    MAX_BLAME_LINES,
    MAX_COUNT_HITS,
    MAX_GREP_HITS,
    MAX_HISTORY,
    MAX_LIST,
    MAX_NOTES,
    MAX_PATTERN_CHARS,
    MAX_READ_LINES,
    MAX_REFERENCES,
    MAX_REQUESTS,
    NOTE_CHARS,
    ROUND_CHARS,
    Rejected,
    Repo,
    _GLOB,
    _SYMBOL,
    _definition_pattern,
    _hits,
    _int,
    _numbered,
)
from knowledge_api import Knowledge, QueryError, construct_label
from review_api import _clip, _run_process, _text

_PROBE_RUNNER = """
import builtins, sys
ALLOWED = {
    "re", "json", "math", "collections", "itertools", "functools",
    "string", "difflib", "statistics", "datetime", "time", "urllib.parse",
}
real_import = builtins.__import__
def safe_import(name, globals=None, locals=None, fromlist=(), level=0):
    if level != 0 or (name not in ALLOWED and name.split(".")[0] not in ALLOWED):
        raise ImportError(f"import {name!r} is not allowed in probe")
    return real_import(name, globals, locals, fromlist, level)
safe_builtins = {k: getattr(builtins, k) for k in (
    "abs", "all", "any", "bool", "chr", "dict", "enumerate", "filter", "float",
    "frozenset", "int", "isinstance", "len", "list", "map", "max", "min", "ord",
    "pow", "print", "range", "repr", "reversed", "round", "set", "slice", "sorted",
    "str", "sum", "tuple", "type", "zip", "Exception", "ValueError", "TypeError",
    "KeyError", "IndexError", "AssertionError",
)}
safe_builtins["__import__"] = safe_import
exec(compile(sys.argv[1], "<probe>", "exec"), {"__builtins__": safe_builtins})
"""

_REFERENCE_KINDS = ("symbol", "callers", "callees", "tests_of")


def _parse_line_range(
    args: dict[str, Any],
    total: int,
    parse_error: str,
    overflow_error: str,
) -> tuple[int, int]:
    try:
        start = max(1, int(args.get("start") or 1))
        end = int(args.get("end") or start + MAX_READ_LINES - 1)
    except (TypeError, ValueError):
        raise Rejected(parse_error) from None
    end = min(end, total, start + MAX_READ_LINES - 1)
    if start > total:
        raise Rejected(overflow_error)
    return start, end


def _resolve_reference_kind(
    repo: Repo, kind: str, args: dict[str, Any]
) -> tuple[tuple[str, str] | None, str, dict[str, Any]]:
    field_name = "name" if kind == "symbol" else "symbol"
    name = args.get(field_name)
    if not isinstance(name, str) or not _SYMBOL.match(name.strip()):
        raise Rejected(f"{kind} takes one identifier in {field_name}")

    reference = getattr(repo, "reference", None)
    answer = reference_module.serve(reference, kind, args) if reference is not None else None
    if answer is not None:
        return answer, kind, args

    # The reference does not know the name: fall back to text lookups (callees needs a definition).
    if kind == "callees":
        raise Rejected(
            f"no definition of {name.strip()} in the reference; ask for definition or grep instead"
        )
    fallback_kind = "definition" if kind == "symbol" else "references"
    return None, fallback_kind, {"symbol": name.strip()}


def _serve_read_diff(repo: Repo, args: dict[str, Any]) -> tuple[str, str]:
    raw_path = args.get("path")
    if not isinstance(raw_path, str) or not raw_path.strip():
        raise Rejected("read_diff takes a file path in path")
    path = raw_path.strip().lstrip("./")
    section = repo.diff_by_file.get(path)
    if section is None:
        raise Rejected(f"{path} is not in the diff")

    diff_lines = section.splitlines()
    start, end = _parse_line_range(
        args,
        len(diff_lines),
        "read_diff takes line numbers in start and end",
        f"diff of {path} has {len(diff_lines)} lines",
    )
    title = f"diff {path}:{start}-{end} (of {len(diff_lines)} lines)"
    return title, "\n".join(diff_lines[start - 1 : end])


def _serve_read(repo: Repo, args: dict[str, Any]) -> tuple[str, str]:
    path = repo.path(args.get("path"))
    lines = repo.lines(path)
    if lines is None:
        raise Rejected(f"{path} is binary or too large to read")

    start, end = _parse_line_range(
        args,
        len(lines),
        "read takes line numbers in start and end",
        f"{path} has {len(lines)} lines",
    )
    return f"read {path}:{start}-{end} (of {len(lines)} lines)", _numbered(lines, start, end)


def _validate_grep_pattern(args: dict[str, Any]) -> str:
    pattern = args.get("pattern")
    invalid_shape = (
        not isinstance(pattern, str)
        or not pattern
        or len(pattern) > MAX_PATTERN_CHARS
        or "\n" in pattern
        or "\0" in pattern
    )
    if invalid_shape:
        raise Rejected(f"grep takes a one-line pattern of at most {MAX_PATTERN_CHARS} characters")
    try:
        re.compile(pattern)
    except re.error as error:
        raise Rejected(f"bad regular expression: {error}") from None
    return pattern


def _validate_grep_pathspec(args: dict[str, Any]) -> tuple[str | None, str | None]:
    glob = args.get("path_glob")
    if glob is None:
        return None, None
    invalid_glob = (
        not isinstance(glob, str)
        or not _GLOB.match(glob)
        or glob.startswith(("/", "-", ":"))
        or ".." in glob.split("/")
    )
    if invalid_glob:
        raise Rejected("path_glob must be a relative glob inside the repository")
    return glob, f":(glob){glob}"


def _grep_limit(args: dict[str, Any]) -> int:
    try:
        return min(MAX_GREP_HITS, max(1, int(args.get("max") or DEFAULT_GREP_HITS)))
    except (TypeError, ValueError):
        return DEFAULT_GREP_HITS


def _grep_note(capped: bool, shown_count: int, total_count: int) -> str:
    if capped:
        return " (more not shown)"
    if shown_count < total_count:
        return f" ({shown_count} shown)"
    return ""


def _filter_grep_hits(
    raw_hits: list[tuple[str, int, str]], start: int, end: int
) -> list[tuple[str, int, str]]:
    if start <= 0 and end <= 0:
        return raw_hits
    low = start or 1
    high = end or 10**9
    return [hit for hit in raw_hits if low <= hit[1] <= high]


def _grep_where(glob: str | None, start: int, end: int) -> str:
    where = f" in {glob}" if glob else ""
    if start > 0 or end > 0:
        where += f" lines {start or 1}-{end or 'end'}"
    return where


def _serve_grep(repo: Repo, args: dict[str, Any]) -> tuple[str, str]:
    pattern = _validate_grep_pattern(args)
    glob, pathspec = _validate_grep_pathspec(args)
    limit = _grep_limit(args)

    # A line window scopes hits to one construct; count_only reports the true count up to MAX_COUNT_HITS.
    start = max(0, _int(args.get("start"), 0))
    end = max(0, _int(args.get("end"), 0))
    needs_wide_fetch = start > 0 or end > 0 or args.get("count_only") is True
    fetch = MAX_COUNT_HITS if needs_wide_fetch else limit

    raw_hits = repo.grep(pattern, pathspec=pathspec, limit=fetch)
    hits = _filter_grep_hits(raw_hits, start, end)
    shown = hits[:limit]
    where = _grep_where(glob, start, end)
    note = _grep_note(len(raw_hits) >= fetch, len(shown), len(hits))
    return f"grep {pattern!r}{where} -> {len(hits)} matches{note}", _hits(shown)


def _validate_list_dir(raw: Any) -> str:
    invalid_dir = (
        not isinstance(raw, str)
        or raw.startswith(("/", "~", "-", ":"))
        or ".." in raw.split("/")
        or "\0" in raw
    )
    if invalid_dir:
        raise Rejected("dir must be a directory inside the repository")
    return posixpath.normpath(raw.strip() or ".")


def _list_dir_entries(files: list[str], prefix: str) -> list[str]:
    names: list[str] = []
    for path in files:
        if not path.startswith(prefix):
            continue
        head, _, rest = path[len(prefix) :].partition("/")
        entry = head + ("/" if rest else "")
        if entry not in names:
            names.append(entry)
    return names


def _serve_list(repo: Repo, args: dict[str, Any]) -> tuple[str, str]:
    directory = _validate_list_dir(args.get("dir"))
    prefix = "" if directory == "." else directory + "/"
    names = _list_dir_entries(repo.files(), prefix)
    if not names:
        raise Rejected(f"{directory} is not a directory with tracked files")

    overflow = f"\n... {len(names) - MAX_LIST} more" if len(names) > MAX_LIST else ""
    return f"list {directory} ({len(names)} entries)", "\n".join(names[:MAX_LIST]) + overflow


def _format_resolved_call(call: Any) -> str:
    enclosing = f" in {call.enclosing}" if call.enclosing else ""
    return f"{call.path}:{call.line}{enclosing}: {_clip(call.text, 140)}"


def _serve_references(repo: Repo, symbol: str) -> tuple[str, str]:
    hits = repo.grep(symbol.rsplit(".", 1)[-1], fixed=True, word=True, limit=MAX_REFERENCES)
    body = _hits(hits)
    reference = getattr(repo, "reference", None)
    if reference is not None and reference.symbols_named(symbol):
        calls = reference.callers(symbol, outside_changes=False)
        if calls:
            resolved_lines = "\n".join(_format_resolved_call(call) for call in calls[:MAX_REFERENCES])
            body = f"Call sites resolved by the reference:\n{resolved_lines}\n\nText matches:\n{body}"

    more = " (more not shown)" if len(hits) >= MAX_REFERENCES else ""
    return f"references {symbol} -> {len(hits)} matches{more}", body


def _near_known(hit: tuple[str, int, str], known: list[Any]) -> bool:
    return any(hit[0] == item.path and abs(hit[1] - item.line) <= 1 for item in known)


def _reference_definitions(
    reference: Any, symbol: str, hits: list[tuple[str, int, str]]
) -> tuple[list[str], list[tuple[str, int, str]]]:
    known = reference.symbols_named(symbol) if reference is not None else []
    if not known:
        return [], hits
    parts = [reference.symbol_view(item, body=True) for item in known[:2]]
    remaining = [hit for hit in hits if not _near_known(hit, known)]
    return parts, remaining


def _serve_definition(repo: Repo, symbol: str) -> tuple[str, str]:
    raw_hits = repo.grep(_definition_pattern(symbol), limit=6)
    reference = getattr(repo, "reference", None)
    parts, hits = _reference_definitions(reference, symbol, raw_hits)

    for path, line, _ in hits[:3]:
        lines = repo.lines(path)
        if lines:
            parts.append(f"{path}:\n{_numbered(lines, line, min(len(lines), line + 24))}")
    if len(hits) > 3:
        parts.append(_hits(hits[3:]))
    return f"definition {symbol} -> {len(hits)} matches", "\n\n".join(parts) or "0 matches"


def _serve_symbol_lookup(repo: Repo, kind: str, args: dict[str, Any]) -> tuple[str, str]:
    symbol = args.get("symbol")
    if not isinstance(symbol, str) or not _SYMBOL.match(symbol):
        raise Rejected("symbol must be one identifier")
    if kind == "references":
        return _serve_references(repo, symbol)
    return _serve_definition(repo, symbol)


def _serve_history(repo: Repo, args: dict[str, Any]) -> tuple[str, str]:
    path = repo.path(args.get("path"))
    count = min(MAX_HISTORY, max(1, _int(args.get("n"), 10)))
    lines = repo.history(path, count)
    return f"history {path} -> {len(lines)} commits", "\n".join(lines) or "no commits found"


def _serve_blame_range(repo: Repo, args: dict[str, Any]) -> tuple[str, str]:
    path = repo.path(args.get("path"))
    file_lines = repo.lines(path)
    total = len(file_lines) if file_lines is not None else 0
    start = max(1, _int(args.get("start"), 1))
    end = min(total, _int(args.get("end"), start), start + MAX_BLAME_LINES - 1)
    if start > total or end < start:
        raise Rejected(f"{path} has {total} lines")
    lines = repo.line_history(path, start, end)
    body = "\n".join(lines) or "no history available for these lines"
    return f"blame_range {path}:{start}-{end} -> {len(lines)} commits", body


def _serve_pickaxe(repo: Repo, args: dict[str, Any]) -> tuple[str, str]:
    text = args.get("string")
    invalid = not isinstance(text, str) or not 3 <= len(text) <= 120 or "\n" in text or "\0" in text
    if invalid:
        raise Rejected("pickaxe takes a one-line string of 3 to 120 characters")
    count = min(MAX_HISTORY, max(1, _int(args.get("n"), 5)))
    lines = repo.pickaxe(text, count)
    body = "\n".join(lines) or "no commit added or removed it"
    return f"pickaxe {text!r} -> {len(lines)} commits", body


def _serve_test_request(
    kind: str, args: dict[str, Any], tests: "testing.TestSession | None"
) -> tuple[str, str]:
    if tests is None:
        raise Rejected("tests are not run in this review")
    try:
        if kind == "run_tests":
            record = tests.run(tests.paths(args.get("paths")), args.get("select"))
        else:
            record = tests.mutation(
                args.get("path"),
                args.get("line"),
                args.get("replacement"),
                args.get("tests"),
            )
    except testing.TestsRejected as error:
        raise Rejected(str(error)) from None
    return f"test run {record['n']}", testing.summarize(record)


def _serve_probe(args: dict[str, Any]) -> tuple[str, str]:
    code = args.get("code")
    if not isinstance(code, str) or not code.strip() or len(code) > 4_000:
        raise Rejected("probe takes a Python snippet in code (1..4000 chars)")
    exit_code, stdout, stderr = _run_process(["python3", "-I", "-c", _PROBE_RUNNER, code], "/tmp", 5.0)
    out = (stdout or "") + (f"\n[stderr]\n{stderr}" if stderr else "")
    out = _clip(out.strip() or "(no output)", 4_000)
    return f"probe (exit {exit_code})", out


def _open_knowledge(spec: dict[str, Any] | None, missing_message: str) -> Knowledge:
    cfg = spec if spec is not None else {}
    knowledge = Knowledge.open(cfg.get("db"), cfg.get("repo"), cfg.get("rev"))
    if knowledge is None:
        raise Rejected(missing_message)
    return knowledge


def _format_note_line(note: Any, alike: Any) -> str:
    where = f"{note.path}:{note.line}" if note.line else note.path
    like = ""
    if alike:
        shared = ", ".join(construct_label(kind) for kind in alike.shared[:3])
        like = f" (shares {shared})"
    return (
        f"#{note.pull} {note.author} on {where}{like} [{note.state}, {note.at[:10]}]: "
        f"{_text(note.body, NOTE_CHARS)}"
    )


def _serve_notes(repo: Repo, kind: str, args: dict[str, Any]) -> tuple[str, str]:
    """Review comments from earlier PRs on this file (`review_notes`) or similar files
    (`like_notes`), read from the knowledge store."""
    path = repo.path(args.get("path"))
    knowledge = _open_knowledge(repo.knowledge, "no review history is available for this repository")
    try:
        if kind == "review_notes":
            notes = [(note, None) for note in knowledge.notes_on(path, limit=MAX_NOTES)]
        else:
            notes = list(knowledge.notes_like(path, limit=MAX_NOTES))
    finally:
        knowledge.close()

    lines = [_format_note_line(note, alike) for note, alike in notes]
    suffix = "" if len(lines) == 1 else "s"
    title = f"{kind} {path} -> {len(lines)} note{suffix}"
    empty = (
        "no earlier review comment on this file"
        if kind == "review_notes"
        else "no addressed review comment on files like this one"
    )
    return title, "\n".join(lines) or empty


def _format_rule_line(rule: Any) -> str:
    note = rule.note
    where = f"{note.path}:{note.line}" if note.line else note.path
    like = ""
    if rule.alike:
        shared = ", ".join(construct_label(kind) for kind in rule.alike.shared[:3])
        like = f" (like this file: {shared})"

    record: list[str] = []
    if rule.missed:
        record.append(f"this reviewer missed it on #{note.pull}")
    if rule.accepted or rule.rejected:
        record.append(f"cited by {rule.accepted} accepted and {rule.rejected} rejected findings")
    badge = f" [{'; '.join(record)}]" if record else ""
    body = _text(note.body, NOTE_CHARS)
    return f"R{rule.id} #{note.pull} {note.author} on {where}{like}{badge}: {body}"


def _serve_rules(repo: Repo, args: dict[str, Any]) -> tuple[str, str]:
    """Rules for a file (`knowledge_api.Knowledge.rules_for`): addressed comments on it
    and similar files, best credited first, each with an `R<n>` id."""
    path = repo.path(args.get("path"))
    knowledge = _open_knowledge(repo.knowledge, "no review history is available for this repository")
    try:
        rules = knowledge.rules_for(path, limit=MAX_NOTES)
    finally:
        knowledge.close()

    lines: list[str] = []
    for rule in rules:
        repo.served_rules.add(rule.id)
        lines.append(_format_rule_line(rule))
    suffix = "" if len(lines) == 1 else "s"
    title = f"rules_for {path} -> {len(lines)} rule{suffix}"
    return title, "\n".join(lines) or "no rule learned for this file or the files like it"


def _serve_graph(repo: Repo, args: dict[str, Any]) -> tuple[str, str]:
    """Run one read-only SQL query over the repository code graph (`Knowledge.query`),
    formatting results as a table."""
    knowledge = _open_knowledge(repo.graph, "no code graph is available for this repository")
    sql = args.get("sql")
    try:
        columns, rows, more = knowledge.query(sql)
    except QueryError as error:
        raise Rejected(f"the query failed: {error}") from None
    finally:
        knowledge.close()

    repo.graph_queries += 1
    shown = " ".join(str(sql).split())
    suffix = "" if len(rows) == 1 else "s"
    more_note = " (more not shown: narrow it or aggregate)" if more else ""
    title = f"graph {_clip(shown, 120)} -> {len(rows)} row{suffix}{more_note}"
    if not rows:
        return title, f"{' | '.join(columns)}\n(no rows)"

    formatted_rows = [
        " | ".join("" if value is None else str(value) for value in row) for row in rows
    ]
    return title, "\n".join([" | ".join(columns), *formatted_rows])


_REPO_HANDLERS: dict[str, Callable[[Repo, dict[str, Any]], tuple[str, str]]] = {
    "probe": lambda _repo, args: _serve_probe(args),
    "read_diff": _serve_read_diff,
    "read": _serve_read,
    "grep": _serve_grep,
    "list": _serve_list,
    "definition": lambda repo, args: _serve_symbol_lookup(repo, "definition", args),
    "references": lambda repo, args: _serve_symbol_lookup(repo, "references", args),
    "history": _serve_history,
    "blame_range": _serve_blame_range,
    "pickaxe": _serve_pickaxe,
    "review_notes": lambda repo, args: _serve_notes(repo, "review_notes", args),
    "like_notes": lambda repo, args: _serve_notes(repo, "like_notes", args),
    "rules_for": _serve_rules,
    "graph": _serve_graph,
}


def serve_request(
    repo: Repo, request: Any, tests: "testing.TestSession | None" = None
) -> tuple[str, str]:
    """Answer one investigator request from the head commit: (title, body).
    Raises Rejected for anything outside the closed set of read-only lookups and tests."""
    if not isinstance(request, dict) or len(request) != 1:
        raise Rejected(
            "a request is an object with exactly one of read, read_diff, grep, list, "
            "definition, references, symbol, callers, callees, tests_of, history, "
            "blame_range, pickaxe, probe, review_notes, rules_for, graph"
        )
    kind, args = next(iter(request.items()))
    if not isinstance(args, dict):
        raise Rejected(f"{kind} takes an object")

    if kind in _REFERENCE_KINDS:
        answer, kind, args = _resolve_reference_kind(repo, kind, args)
        if answer is not None:
            return answer

    handler = _REPO_HANDLERS.get(kind)
    if handler is not None:
        return handler(repo, args)
    if kind in ("run_tests", "mutation_check"):
        return _serve_test_request(kind, args, tests)
    raise Rejected(
        f"unknown request {kind!r}: use read, grep, list, definition, references, history, "
        "blame_range, pickaxe, review_notes, rules_for or graph"
    )


def serve_blocks(
    repo: Repo,
    requests: Any,
    limit: int = ROUND_CHARS,
    tests: "testing.TestSession | None" = None,
) -> tuple[list[tuple[str, str]], int, int]:
    """Answer a round of requests within the round's size limit: ([(title, block)], served, rejected)."""
    out: list[tuple[str, str]] = []
    size = served = rejected = 0
    items = requests if isinstance(requests, list) else []
    for request in items[:MAX_REQUESTS]:
        try:
            title, body = serve_request(repo, request, tests)
            served += 1
        except Rejected as error:
            title, body = f"rejected {json.dumps(request)[:160]}", str(error)
            rejected += 1
        block = f"## {title}\n{body}"
        if size + len(block) > limit:
            block = (
                block[: max(0, limit - size)]
                + "\n... cut: the round's size limit is reached; ask for less at a time"
            )
        out.append((title, block))
        size += len(block)
        if size >= limit:
            break
    if len(items) > MAX_REQUESTS:
        overflow = len(items) - MAX_REQUESTS
        out.append(
            (
                "not served",
                f"## not served\n{overflow} requests beyond the {MAX_REQUESTS} allowed per round",
            )
        )
    return out, served, rejected


def serve(
    repo: Repo,
    requests: Any,
    limit: int = ROUND_CHARS,
    tests: "testing.TestSession | None" = None,
) -> tuple[str, int, int]:
    """Answer a round of requests within the round's size limit: (text, served, rejected)."""
    blocks, served, rejected = serve_blocks(repo, requests, limit, tests)
    return "\n\n".join(block for _title, block in blocks), served, rejected
