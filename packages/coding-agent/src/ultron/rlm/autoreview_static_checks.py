"""Deterministic static-stage checks and diff-orphan detection for `autoreview_static`."""
from __future__ import annotations

import importlib
import importlib.util
import json
import os
import re
import time
import tomllib
from typing import Any, Callable

from review_api import _DEF, FileDiff, Git, Scope, _text, file_kind

TOOL_TIMEOUT_S = 60.0
MAX_STAGE_FINDINGS = 12
RUFF_RULES = "F821,F822,F823,F632,F701,F702,F704,F706"
RUFF_NIT_RULES = "F401,F841,F541,F601,F811,E711,E712,E713,E714,E721"
LOKI_SECURITY_RULES = frozenset({
    "secret",
    "actions-injection",
    "sql-injection",
    "command-injection",
    "tls-verification",
    "zip-slip",
    "open-redirect",
    "ssrf",
    "request-authorization",
    "unsafe-elixir-eval",
})
LOKI_NIT_RULES = frozenset({
    "no-debugger",
    "no-empty",
    "no-focused-tests",
    "no-disabled-tests",
    "unexplained-allow",
    "unexplained-credo-disable",
    "unsafe-without-safety-comment",
    "placeholder",
    "elixir-list-slop",
    "python-fixed-sleep",
    "rust-fixed-sleep",
    "elixir-fixed-sleep",
})
STAGES = ("compile", "lint", "typecheck", "loki")
LOKI_BATCH_BYTES = 512 * 1024
MAX_ORPHAN_FINDINGS = 8

Which = Callable[[str], "str | None"]
Executor = Callable[[list[str], str, dict[str, str], float], "tuple[int, str]"]
EnvFn = Callable[[], dict[str, str]]
ErrorParser = Callable[[str], "tuple[int, str]"]

_LOKI_LINE = re.compile(r"^(?P<path>.+?):(?P<line>\d+): (?P<rule>[\w./-]+): (?P<message>.*)$")
_MYPY_LINE = re.compile(
    r"^(?P<path>[^:\n]+):(?P<line>\d+):(?:\d+:)? error: (?P<message>.*?)"
    r"(?:\s+\[(?P<code>[\w-]+)\])?$",
    re.M,
)
_NODE_LINE = re.compile(r":(\d+)\s*$", re.M)
_GOFMT_LINE = re.compile(r":(\d+):\d+:\s*(.+)")
_SHELL_LINE = re.compile(r"line (\d+):\s*(.+)")
_PRIVATE_CALL = re.compile(r"\b(_[A-Za-z][A-Za-z0-9_]{2,})\s*\(")
_INPROCESS_SUFFIXES = (".py", ".json", ".toml", ".yml", ".yaml")
_SEVERITY_BY_LEVEL = {"critical": "blocker", "high": "major", "nit": "nit"}


def _new_lines(item: FileDiff) -> set[int]:
    lines: set[int] = set()
    for hunk in item.hunks:
        for line in hunk.lines:
            if line.kind == "+" and line.new is not None:
                lines.add(line.new)
    return lines


def _text_of(lines: list[str] | None) -> str | None:
    if lines is None:
        return None
    suffix = "\n" if lines else ""
    return "\n".join(lines) + suffix


def _finding(
    stage: str,
    path: str,
    line: int,
    claim: str,
    detail: str,
    *,
    level: str,
    category: str,
    tool: str,
    host_confirmed: bool = True,
) -> dict[str, Any]:
    """Build a static finding; pattern scanners leave `host_confirmed=False` for verification."""
    trimmed_detail = _text(detail, 300)
    if host_confirmed:
        why = f"{tool} reports it on a line this change added: {trimmed_detail}"
        scenario = f"{tool} fails on {path}:{line} at the reviewed commit."
    else:
        why = (
            f"{tool} matched a pattern on an added line; confirm in context that this is a "
            "real issue (not a test constant, ORM expression, already-validated input, or "
            f"intentional fail-open): {trimmed_detail}"
        )
        scenario = (
            f"{tool} flagged {path}:{line}; verify whether the pattern is genuinely buggy "
            "or exploitable."
        )

    severity = _SEVERITY_BY_LEVEL.get(level, "minor")
    entry: dict[str, Any] = {
        "file": path,
        "line": line,
        "level": level,
        "severity": severity,
        "finder_level": level,
        "category": category,
        "claim": claim,
        "why": why,
        "scenario": scenario,
        "suggested_fix": "",
        "confidence": 1.0 if host_confirmed else 0.75,
        "reviewers": [f"static:{stage}"],
        "source": f"static:{stage}",
        "citations": [],
        "host_confirmed": host_confirmed,
    }
    if host_confirmed:
        entry["verified_by"] = f"static:{stage}"
        entry["evidence"] = _text(detail, 400)
        entry["how_verified"] = f"{tool} run by the host on the reviewed commit's {path}"
    return entry


class _Stage:
    def __init__(self, name: str) -> None:
        self.name = name
        self.status = "clean"
        self.note: str | None = None
        self.files: list[str] = []
        self.findings: list[dict[str, Any]] = []
        self.flagged: set[str] = set()
        self.ms = 0

    def skip(self, note: str) -> None:
        self.status, self.note = "skipped", note

    def fail(self, error: BaseException | str) -> None:
        self.status = "failed"
        raw = error if isinstance(error, str) else f"{type(error).__name__}: {error}"
        self.note = _text(raw, 200)
        self.findings = []

    def add(self, finding: dict[str, Any]) -> None:
        if len(self.findings) < MAX_STAGE_FINDINGS:
            self.findings.append(finding)

    def record(self) -> dict[str, Any]:
        status = "findings" if self.status == "clean" and self.findings else self.status
        return {
            "name": self.name,
            "status": status,
            "note": self.note,
            "files": list(self.files),
            "findings": len(self.findings),
            "ms": self.ms,
        }


def _broken_python(path: str, text: str) -> tuple[int, str] | None:
    try:
        compile(text, path, "exec", dont_inherit=True)
    except SyntaxError as error:
        lineno = error.lineno if error.lineno is not None else 1
        return max(1, lineno), f"{type(error).__name__}: {error.msg}"
    except ValueError as error:
        return 1, f"ValueError: {error}"
    return None


def _broken_json(_path: str, text: str) -> tuple[int, str] | None:
    try:
        json.loads(text)
    except ValueError as error:
        lineno = getattr(error, "lineno", 1)
        message = getattr(error, "msg", error)
        return max(1, lineno), f"invalid JSON: {message}"
    return None


def _broken_toml(_path: str, text: str) -> tuple[int, str] | None:
    try:
        tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        found = re.search(r"line (\d+)", str(error))
        lineno = int(found.group(1)) if found else 1
        return lineno, f"invalid TOML: {error}"
    return None


def _broken_yaml(_path: str, text: str) -> tuple[int, str] | None:
    if importlib.util.find_spec("yaml") is None:
        return None
    yaml = importlib.import_module("yaml")

    try:
        list(yaml.safe_load_all(text))
    except yaml.YAMLError as error:
        mark = getattr(error, "problem_mark", None)
        lineno = (mark.line + 1) if mark is not None else 1
        problem = getattr(error, "problem", error)
        return max(1, lineno), f"invalid YAML: {problem}"
    return None


_SYNTAX_CHECKERS = (
    ((".py",), _broken_python),
    ((".json",), _broken_json),
    ((".toml",), _broken_toml),
    ((".yml", ".yaml"), _broken_yaml),
)


def _broken(path: str, text: str) -> tuple[int, str] | None:
    """Return `(line, message)` of the first syntax error, or None when it parses."""
    for suffixes, checker in _SYNTAX_CHECKERS:
        if path.endswith(suffixes):
            return checker(path, text)
    return None


def _broken_external_base(
    path: str,
    base: str,
    argv: list[str],
    workdir: str,
    executor: Executor,
    env_fn: EnvFn,
) -> bool:
    """Whether the base commit's version of a file also fails an external syntax check."""
    scratch = os.path.join(workdir, ".base", path)
    os.makedirs(os.path.dirname(scratch), exist_ok=True)
    with open(scratch, "w", encoding="utf-8") as handle:
        handle.write(base)

    code, _ = executor([*argv, scratch], workdir, env_fn(), TOOL_TIMEOUT_S)
    return code != 0


def _parse_node_error(output: str) -> tuple[int, str]:
    found = _NODE_LINE.search(output.split("\n\n")[0])
    lineno = int(found.group(1)) if found else 1
    message = next(
        (row for row in output.splitlines() if "Error" in row),
        output.strip()[:200],
    )
    return lineno, message


def _parse_gofmt_error(output: str) -> tuple[int, str]:
    match = _GOFMT_LINE.search(output)
    if match is None:
        return 1, output.strip()[:200]
    return int(match.group(1)), match.group(2).strip()


def _parse_shell_error(output: str) -> tuple[int, str]:
    match = _SHELL_LINE.search(output)
    if match is None:
        return 1, output.strip()[:200]
    return int(match.group(1)), match.group(2).strip()


_EXTERNAL_SPECS: tuple[
    tuple[tuple[str, ...], str, str, str, str, ErrorParser],
    ...,
] = (
    ((".js", ".mjs", ".cjs"), "node", "--check", "node --check", "no longer parses", _parse_node_error),
    ((".go",), "gofmt", "-e", "gofmt", "no longer parses", _parse_gofmt_error),
    ((".sh", ".bash"), "shell", "-n", "", "has a shell syntax error", _parse_shell_error),
)


def _external_compile_spec(
    path: str,
    tools: dict[str, str | None],
) -> tuple[list[str], str, str, ErrorParser] | None:
    for suffixes, tool_key, flag, label, verb, parser in _EXTERNAL_SPECS:
        if path.endswith(suffixes):
            binary = tools[tool_key]
            if binary is None:
                return None
            tool_name = label if label else os.path.basename(binary)
            return [binary, flag], tool_name, verb, parser
    return None


def _check_external_compile(
    stage: _Stage,
    path: str,
    base: str | None,
    spec: tuple[list[str], str, str, ErrorParser],
    workdir: str,
    executor: Executor,
    env_fn: EnvFn,
) -> None:
    prefix, tool_name, verb, parser = spec
    target = os.path.join(workdir, path)
    code, output = executor([*prefix, target], workdir, env_fn(), TOOL_TIMEOUT_S)
    stage.files.append(path)
    if code == 0:
        return

    stage.flagged.add(path)
    if base is not None and _broken_external_base(
        path, base, prefix, workdir, executor, env_fn
    ):
        return

    lineno, message = parser(output)
    claim = f"{path} {verb}: {_text(message, 160)}"
    finding = _finding(
        "compile",
        path,
        lineno,
        claim,
        message,
        level="high",
        category="correctness",
        tool=tool_name,
    )
    stage.add(finding)


def _check_inprocess_compile(
    stage: _Stage,
    path: str,
    text: str,
    base: str | None,
) -> None:
    stage.files.append(path)
    error = _broken(path, text)
    if error is None:
        return

    stage.flagged.add(path)
    if base is not None and _broken(path, base) is not None:
        return

    lineno, message = error
    is_python = path.endswith(".py")
    action = "compiles" if is_python else "parses"
    tool = "the compiler" if is_python else "the parser"
    claim = f"{path} no longer {action}: {_text(message, 160)} (line {lineno})."
    finding = _finding(
        "compile",
        path,
        lineno,
        claim,
        message,
        level="high",
        category="correctness",
        tool=tool,
    )
    stage.add(finding)


def _compile_one(
    stage: _Stage,
    path: str,
    text: str,
    base: str | None,
    tools: dict[str, str | None],
    workdir: str,
    executor: Executor,
    env_fn: EnvFn,
) -> None:
    external = _external_compile_spec(path, tools)
    if external is not None:
        _check_external_compile(stage, path, base, external, workdir, executor, env_fn)
        return
    if path.endswith(_INPROCESS_SUFFIXES):
        _check_inprocess_compile(stage, path, text, base)


def run_compile_stage(
    stage: _Stage,
    heads: dict[str, str],
    bases: dict[str, str | None],
    workdir: str,
    which: Which,
    executor: Executor,
    env_fn: EnvFn,
) -> None:
    tools = {
        "node": which("node"),
        "gofmt": which("gofmt"),
        "shell": which("bash") or which("sh"),
    }
    for path, text in heads.items():
        _compile_one(
            stage,
            path,
            text,
            bases.get(path),
            tools,
            workdir,
            executor,
            env_fn,
        )

    if not stage.files:
        suffix = "" if tools["node"] else " (node is not installed for JavaScript)"
        stage.skip("no changed file it checks" + suffix)


def _parsing_python(heads: dict[str, str]) -> list[str]:
    """Changed Python files that compile cleanly."""
    return [
        path
        for path, text in heads.items()
        if path.endswith(".py") and _broken(path, text) is None
    ]


def _run_ruff(
    ruff: str,
    python_files: list[str],
    workdir: str,
    executor: Executor,
    env_fn: EnvFn,
) -> tuple[list[dict[str, Any]] | None, str | None]:
    argv = [
        ruff,
        "check",
        "--isolated",
        "--no-cache",
        "--select",
        f"{RUFF_RULES},{RUFF_NIT_RULES}",
        "--output-format",
        "json",
        "--",
        *python_files,
    ]
    code, output = executor(argv, workdir, env_fn(), TOOL_TIMEOUT_S)
    if code not in (0, 1):
        return None, f"ruff exited {code}: {_text(output, 160)}"

    try:
        reported = json.loads(output[output.index("[") :]) if "[" in output else []
    except ValueError:
        return None, f"ruff's output was not JSON: {_text(output, 160)}"
    return reported, None


def _ruff_location(item: dict[str, Any], workdir: str) -> tuple[str, int]:
    raw_name = item.get("filename")
    filename = str(raw_name) if raw_name else ""
    path = os.path.relpath(filename, workdir) if os.path.isabs(filename) else filename
    location = item.get("location")
    row = location.get("row") if isinstance(location, dict) else 0
    return path, int(row or 0)


def _record_ruff_item(
    stage: _Stage,
    item: dict[str, Any],
    added: dict[str, set[int]],
    workdir: str,
    error_codes: set[str],
    nit_codes: set[str],
) -> None:
    path, lineno = _ruff_location(item, workdir)
    raw_code = item.get("code")
    rule = str(raw_code) if raw_code else ""
    if rule in error_codes:
        stage.flagged.add(path)

    known_rule = rule in error_codes or rule in nit_codes
    if not known_rule or lineno not in added.get(path, set()):
        return

    is_nit = rule in nit_codes
    raw_message = item.get("message")
    message = f"{rule}: {raw_message}"
    claim = f"{_text(raw_message, 160)} ({rule})."
    level = "nit" if is_nit else "high"
    category = "maintainability" if is_nit else "correctness"
    stage.add(
        _finding("lint", path, lineno, claim, message, level=level, category=category, tool="ruff")
    )


def run_lint_stage(
    stage: _Stage,
    heads: dict[str, str],
    added: dict[str, set[int]],
    workdir: str,
    which: Which,
    executor: Executor,
    env_fn: EnvFn,
) -> None:
    python_files = _parsing_python(heads)
    if not python_files:
        stage.skip("no changed Python file that compiles")
        return

    ruff = which("ruff")
    if ruff is None:
        stage.skip("ruff is not installed")
        return

    reported, failure = _run_ruff(ruff, python_files, workdir, executor, env_fn)
    if failure is not None or reported is None:
        stage.fail(failure or "ruff failed")
        return

    error_codes = set(RUFF_RULES.split(","))
    nit_codes = set(RUFF_NIT_RULES.split(","))
    stage.files = python_files
    for item in reported:
        _record_ruff_item(stage, item, added, workdir, error_codes, nit_codes)


def run_typecheck_stage(
    stage: _Stage,
    heads: dict[str, str],
    added: dict[str, set[int]],
    workdir: str,
    which: Which,
    executor: Executor,
    env_fn: EnvFn,
) -> None:
    python_files = _parsing_python(heads)
    if not python_files:
        stage.skip("no changed Python file that compiles")
        return

    mypy = which("mypy")
    if mypy is None:
        stage.skip("mypy is not installed")
        return

    argv = [
        mypy,
        "--config-file=",
        "--ignore-missing-imports",
        "--follow-imports=skip",
        "--no-incremental",
        "--cache-dir=/dev/null",
        "--no-error-summary",
        "--show-error-codes",
        "--",
        *python_files,
    ]
    code, output = executor(argv, workdir, env_fn(), TOOL_TIMEOUT_S)
    if code not in (0, 1):
        stage.fail(f"mypy exited {code}: {_text(output, 160)}")
        return

    stage.files = python_files
    for match in _MYPY_LINE.finditer(output):
        path = match.group("path").strip()
        lineno = int(match.group("line"))
        stage.flagged.add(path)
        if lineno not in added.get(path, set()):
            continue

        code_tag = f" [{match.group('code')}]" if match.group("code") else ""
        message = match.group("message") + code_tag
        claim = f"Type error: {_text(match.group('message'), 160)}."
        stage.add(
            _finding(
                "typecheck",
                path,
                lineno,
                claim,
                message,
                level="medium",
                category="correctness",
                tool="mypy",
            )
        )


def _loki_batches(
    heads: dict[str, str],
    bases: dict[str, str | None],
    max_bytes: int = LOKI_BATCH_BYTES,
) -> tuple[list[list[list[str]]], list[str], list[str]]:
    """Split changed files into JSON batches safely under the 1 MiB kernel frame cap."""
    batches: list[list[list[str]]] = []
    current: list[list[str]] = []
    current_bytes = 32
    scanned: list[str] = []
    oversized: list[str] = []

    for path, text in heads.items():
        base_text = bases.get(path)
        triple = [path, "" if base_text is None else base_text, text]
        entry_bytes = len(json.dumps(triple).encode("utf-8")) + 2
        if entry_bytes > max_bytes:
            oversized.append(path)
            continue

        if current and current_bytes + entry_bytes > max_bytes:
            batches.append(current)
            current = []
            current_bytes = 32

        current.append(triple)
        current_bytes += entry_bytes
        scanned.append(path)

    if current:
        batches.append(current)
    return batches, scanned, oversized


async def _fetch_loki_violations(
    stage: _Stage,
    request: Any,
    batches: list[list[list[str]]],
) -> list[Any] | None:
    violations: list[Any] = []
    for batch in batches:
        try:
            reply = await request("review.loki", {"changes": batch})
        except (RuntimeError, OSError, ValueError) as error:
            stage.skip(f"the Loki scan is not available ({_text(str(error), 120)})")
            return None

        if isinstance(reply, dict) and isinstance(reply.get("violations"), list):
            violations.extend(reply["violations"])
    return violations


def _loki_classification(rule: str) -> tuple[str, str]:
    short_rule = rule.removeprefix("loki/")
    if short_rule in LOKI_SECURITY_RULES:
        return "high", "security"
    is_nit = short_rule in LOKI_NIT_RULES or short_rule.startswith(("hygiene-", "nits-"))
    if is_nit:
        return "nit", "maintainability"
    return "medium", "correctness"


def _add_loki_violation(
    stage: _Stage,
    violation: Any,
    added: dict[str, set[int]],
) -> None:
    match = _LOKI_LINE.match(str(violation))
    if match is None:
        return

    path = match.group("path")
    lineno = int(match.group("line"))
    rule = match.group("rule")
    stage.flagged.add(path)
    if lineno not in added.get(path, set()):
        return

    level, category = _loki_classification(rule)
    claim = f"{_text(match.group('message'), 200)} ({rule})."
    finding = _finding(
        "loki",
        path,
        lineno,
        claim,
        str(violation),
        level=level,
        category=category,
        tool=f"Loki ({rule})",
        host_confirmed=False,
    )
    stage.add(finding)


async def run_loki_stage(
    stage: _Stage,
    rlm: Any,
    heads: dict[str, str],
    bases: dict[str, str | None],
    added: dict[str, set[int]],
) -> None:
    request = getattr(rlm, "host_request", None)
    if request is None:
        stage.skip("the host serves no Loki scan")
        return

    batches, scanned, oversized = _loki_batches(heads, bases)
    if not batches:
        stage.skip("every changed file exceeded the Loki frame size cap")
        return

    violations = await _fetch_loki_violations(stage, request, batches)
    if violations is None:
        return

    stage.files = scanned
    if oversized:
        stage.note = f"skipped {len(oversized)} oversized file(s) over 512 KiB"

    for violation in violations:
        _add_loki_violation(stage, violation, added)


def collect_static_inputs(
    files: list[FileDiff],
    read_head: Callable[[str], list[str] | None],
    read_base: Callable[[str], list[str] | None],
) -> tuple[dict[str, str], dict[str, str | None], dict[str, set[int]]]:
    heads: dict[str, str] = {}
    bases: dict[str, str | None] = {}
    added: dict[str, set[int]] = {}
    for item in files:
        if item.status == "deleted" or item.binary:
            continue

        text = _text_of(read_head(item.path))
        if text is None:
            continue

        heads[item.path] = text
        base_path = item.old_path or item.path
        bases[item.path] = None if item.status == "added" else _text_of(read_base(base_path))
        added[item.path] = _new_lines(item)
    return heads, bases, added


def populate_workdir(workdir: str, heads: dict[str, str]) -> None:
    real_workdir = os.path.realpath(workdir) + os.sep
    for path, text in heads.items():
        target = os.path.join(workdir, path)
        if not os.path.realpath(target).startswith(real_workdir):
            continue

        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "w", encoding="utf-8") as handle:
            handle.write(text)


def run_sync_stage(stage: _Stage, runner: Callable[[_Stage], None]) -> None:
    started = time.monotonic()
    try:
        runner(stage)
    except Exception as error:
        stage.fail(error)
    stage.ms = int((time.monotonic() - started) * 1000)


def format_static_result(stages: dict[str, _Stage]) -> dict[str, Any]:
    findings: list[dict[str, Any]] = []
    clean: dict[str, list[str]] = {}
    for stage in stages.values():
        if stage.status != "clean":
            continue

        findings.extend(stage.findings)
        for path in stage.files:
            if path not in stage.flagged:
                clean.setdefault(path, []).append(stage.name)

    return {
        "stages": [stage.record() for stage in stages.values()],
        "findings": findings,
        "clean": clean,
    }


def _record_removed_calls(text: str, removed: set[str]) -> None:
    for match in _PRIVATE_CALL.finditer(text):
        name = match.group(1)
        if not name.startswith("__"):
            removed.add(name)


def _scan_hunk_calls(hunk: Any, removed: set[str], added: set[str]) -> None:
    for line in hunk.lines:
        if line.kind == "-":
            _record_removed_calls(line.text, removed)
        elif line.kind == "+":
            for match in _PRIVATE_CALL.finditer(line.text):
                added.add(match.group(1))


def _diff_private_calls(item: FileDiff) -> tuple[set[str], set[str]]:
    removed: set[str] = set()
    added: set[str] = set()
    for hunk in item.hunks:
        _scan_hunk_calls(hunk, removed, added)
    return removed, added


def _collect_orphan_candidates(scope: Scope) -> list[str]:
    candidates: list[str] = []
    for item in scope.files:
        if item.status == "deleted" or item.binary or file_kind(item.path) == "test":
            continue

        removed, added = _diff_private_calls(item)
        for name in sorted(removed - added):
            if name not in candidates:
                candidates.append(name)
            if len(candidates) >= 15:
                return candidates
    return candidates


def _parse_git_grep_hits(stdout: str, grep_rev: str) -> list[tuple[str, int, str]]:
    hits: list[tuple[str, int, str]] = []
    prefix = f"{grep_rev}:"
    for raw in stdout.splitlines():
        line = raw[len(prefix) :] if grep_rev and raw.startswith(prefix) else raw
        path, _, rest = line.partition(":")
        num_str, _, text = rest.partition(":")
        if num_str.isdigit():
            hits.append((path, int(num_str), text))
    return hits


def _is_undecorated_def(
    scope: Scope,
    path: str,
    line_num: int,
    text: str,
    name: str,
) -> bool:
    if file_kind(path) == "test":
        return False

    match = _DEF.match(text)
    if not match or (match.group(1) or match.group(2)) != name:
        return False

    src = scope.read_file(path)
    if src is None:
        return True
    if line_num >= 2 and src[line_num - 2].lstrip().startswith("@"):
        return False
    return True


def _check_orphan_candidate(git: Git, scope: Scope, name: str) -> dict[str, Any] | None:
    rev_arg = scope.grep_rev if scope.grep_rev else "--untracked"
    code, stdout, _ = git.call(
        "grep",
        "-n",
        "-w",
        "-I",
        "--max-count=3",
        "-e",
        name,
        rev_arg,
        "--",
        ".",
        ":(exclude)*.lock",
        ":(exclude)*.min.js",
    )
    if code != 0:
        return None

    hits = _parse_git_grep_hits(stdout, scope.grep_rev)
    if len(hits) != 1:
        return None

    path, line_num, text = hits[0]
    if not _is_undecorated_def(scope, path, line_num, text, name):
        return None

    detail = f"{path}:{line_num}: {text.strip()}"
    claim = (
        f"Private helper `{name}` has no remaining callers after this change; "
        "delete it instead of leaving dead code."
    )
    return _finding(
        "orphans",
        path,
        line_num,
        claim,
        detail,
        level="nit",
        category="maintainability",
        tool="orphan-check",
    )


def find_orphaned_private_helpers(git: Git | None, scope: Scope) -> list[dict[str, Any]]:
    """Find private helpers whose call sites were removed and only their definition remains."""
    if git is None or scope.grep_rev == "":
        return []

    candidates = _collect_orphan_candidates(scope)
    if not candidates:
        return []

    findings: list[dict[str, Any]] = []
    for name in candidates:
        finding = _check_orphan_candidate(git, scope, name)
        if finding is None:
            continue

        findings.append(finding)
        if len(findings) >= MAX_ORPHAN_FINDINGS:
            break
    return findings
