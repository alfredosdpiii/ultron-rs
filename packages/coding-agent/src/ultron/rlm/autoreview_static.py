"""The review's deterministic stage: plain-code checks of the changed files before any model judges a finding.

Four stages, each degrading on its own (a missing tool or a failure skips that stage, says why, and leaves the
others standing):

- `compile`: Python is compiled in-process, JSON and TOML are parsed; JavaScript is checked with `node --check`
  when node is installed.
- `lint`: `ruff` (when installed) for the error-class rules only: undefined names, misused `return`/`yield`/`break`.
- `typecheck`: `mypy` (when installed) on the changed Python files alone, imports it cannot follow taken as untyped.
- `loki`: Loki's core rule pack over each changed file, before and after (host request `review.loki`): net-new
  credentials, merge-conflict markers, workflow script injection and the language packs.

The tools run on copies of the reviewed commit's files in a private temporary directory, with their own
configuration ignored (`ruff --isolated`, `mypy --config-file=`): the reviewed repository never configures them,
so it cannot make them run its code. Only problems on lines the change added are findings; they are confirmed by
the host (no verifier frame). A stage that ran clean on a file contradicts a model's claim it settles (a syntax
error where the file compiles, an undefined name where the linter found none): such findings are dropped before
the verifier sees them ([`contradiction`]).
"""
from __future__ import annotations

import json
import os
import re
import shutil
import tempfile
import time
import tomllib
from typing import Any, Callable

import autoreview_tests as testing
from review_api import FileDiff, _text

#: Seconds one external tool may take.
TOOL_TIMEOUT_S = 60.0
#: Findings one stage contributes at most.
MAX_STAGE_FINDINGS = 12
#: Ruff's error-class rules: undefined and unbound names, misplaced return/yield/break/continue, invalid comparisons.
RUFF_RULES = "F821,F822,F823,F632,F701,F702,F704,F706"
STAGES = ("compile", "lint", "typecheck", "loki")

Which = Callable[[str], "str | None"]
Executor = Callable[[list[str], str, dict[str, str], float], tuple[int, str]]

_LOKI_LINE = re.compile(r"^(?P<path>.+?):(?P<line>\d+): (?P<rule>[\w./-]+): (?P<message>.*)$")
_MYPY_LINE = re.compile(r"^(?P<path>[^:\n]+):(?P<line>\d+):(?:\d+:)? error: (?P<message>.*?)(?:\s+\[(?P<code>[\w-]+)\])?$",
                        re.M)
_NODE_LINE = re.compile(r":(\d+)\s*$", re.M)

#: Claims a stage that ran clean on the file contradicts: (stage, pattern).
CONTRADICTED_BY = (
    ("compile", re.compile(r"\b(syntax ?error|indentation ?error|invalid syntax|parse error|unparse?able|"
                           r"(does not|doesn't|will not|won't|fails? to|cannot|can't) (compile|parse|be parsed))\b", re.I)),
    ("lint", re.compile(r"\b(name ?error|undefined (name|variable|function)|(is|are) not defined|unbound (local|name)|"
                        r"unresolved (name|reference))\b", re.I)),
    ("typecheck", re.compile(r"\b(type[- ]?check(ing|er)? (fails|error|errors)|mypy (error|fails|reports))\b", re.I)),
)


def _new_lines(item: FileDiff) -> set[int]:
    return {line.new for hunk in item.hunks for line in hunk.lines if line.kind == "+" and line.new is not None}


def _text_of(lines: list[str] | None) -> str | None:
    return None if lines is None else "\n".join(lines) + ("\n" if lines else "")


def _finding(stage: str, path: str, line: int, claim: str, detail: str, *, level: str, category: str,
             tool: str) -> dict[str, Any]:
    """A finding the host confirmed by running `tool` itself."""
    return {
        "file": path, "line": line, "level": level, "severity": {"critical": "blocker", "high": "major"}.get(level, "minor"),
        "finder_level": level, "category": category, "claim": claim,
        "why": f"{tool} reports it on a line this change added: {_text(detail, 300)}",
        "scenario": f"{tool} fails on {path}:{line} at the reviewed commit.", "suggested_fix": "",
        "confidence": 1.0, "reviewers": [f"static:{stage}"], "source": f"static:{stage}", "citations": [],
        "host_confirmed": True, "verified_by": f"static:{stage}", "evidence": _text(detail, 400),
        "how_verified": f"{tool} run by the host on the reviewed commit's {path}",
    }


class _Stage:
    def __init__(self, name: str) -> None:
        self.name = name
        self.status = "clean"
        self.note: str | None = None
        self.files: list[str] = []
        self.findings: list[dict[str, Any]] = []
        #: Files the tool found anything in, on an added line or not: never clean.
        self.flagged: set[str] = set()
        self.ms = 0

    def skip(self, note: str) -> None:
        self.status, self.note = "skipped", note

    def fail(self, error: BaseException | str) -> None:
        self.status = "failed"
        self.note = _text(error if isinstance(error, str) else f"{type(error).__name__}: {error}", 200)
        self.findings = []

    def add(self, finding: dict[str, Any]) -> None:
        if len(self.findings) < MAX_STAGE_FINDINGS:
            self.findings.append(finding)

    def record(self) -> dict[str, Any]:
        status = "findings" if self.status == "clean" and self.findings else self.status
        return {"name": self.name, "status": status, "note": self.note, "files": list(self.files),
                "findings": len(self.findings), "ms": self.ms}


def _broken(path: str, text: str) -> tuple[int, str] | None:
    """(line, message) of the first syntax error of a Python, JSON or TOML file; None when it parses."""
    if path.endswith(".py"):
        try:
            compile(text, path, "exec", dont_inherit=True)
        except SyntaxError as error:
            return max(1, error.lineno or 1), f"{type(error).__name__}: {error.msg}"
        except ValueError as error:  # null bytes
            return 1, f"ValueError: {error}"
    elif path.endswith(".json"):
        try:
            json.loads(text)
        except ValueError as error:
            return max(1, getattr(error, "lineno", 1)), f"invalid JSON: {getattr(error, 'msg', error)}"
    elif path.endswith(".toml"):
        try:
            tomllib.loads(text)
        except tomllib.TOMLDecodeError as error:
            found = re.search(r"line (\d+)", str(error))
            return int(found.group(1)) if found else 1, f"invalid TOML: {error}"
    return None


def _compile(stage: _Stage, heads: dict[str, str], bases: dict[str, str | None], workdir: str, which: Which,
             executor: Executor) -> None:
    node = which("node")
    for path, text in heads.items():
        if path.endswith((".js", ".mjs", ".cjs")):
            if node is None:
                continue
            code, output = executor([node, "--check", os.path.join(workdir, path)], workdir, testing.sandbox_env(), TOOL_TIMEOUT_S)
            stage.files.append(path)
            if code != 0:
                stage.flagged.add(path)
                found = _NODE_LINE.search(output.split("\n\n")[0])
                line = int(found.group(1)) if found else 1
                message = next((row for row in output.splitlines() if "Error" in row), output.strip()[:200])
                base = bases.get(path)
                if base is None or broken_js_base(path, base, node, workdir, executor) is False:
                    stage.add(_finding("compile", path, line, f"{path} no longer parses: {_text(message, 160)}",
                                       message, level="high", category="correctness", tool="node --check"))
            continue
        if not path.endswith((".py", ".json", ".toml")):
            continue
        stage.files.append(path)
        error = _broken(path, text)
        if error is None:
            continue
        stage.flagged.add(path)
        base = bases.get(path)
        # Broken before the change too: not this change's problem.
        if base is not None and _broken(path, base) is not None:
            continue
        line, message = error
        stage.add(_finding("compile", path, line, f"{path} no longer {'compiles' if path.endswith('.py') else 'parses'}: "
                           f"{_text(message, 160)} (line {line}).", message, level="high", category="correctness",
                           tool="the compiler" if path.endswith(".py") else "the parser"))
    if not stage.files:
        stage.skip("no changed file it checks" + ("" if node else " (node is not installed for JavaScript)"))


def broken_js_base(path: str, base: str, node: str, workdir: str, executor: Executor) -> bool:
    """Whether the base commit's version of a JavaScript file failed `node --check` as well."""
    scratch = os.path.join(workdir, ".base", path)
    os.makedirs(os.path.dirname(scratch), exist_ok=True)
    with open(scratch, "w", encoding="utf-8") as handle:
        handle.write(base)
    code, _ = executor([node, "--check", scratch], workdir, testing.sandbox_env(), TOOL_TIMEOUT_S)
    return code != 0


def _parsing_python(heads: dict[str, str]) -> list[str]:
    """The changed Python files that compile: a linter or type checker says nothing true about the others."""
    return [path for path, text in heads.items() if path.endswith(".py") and _broken(path, text) is None]


def _lint(stage: _Stage, heads: dict[str, str], added: dict[str, set[int]], workdir: str, which: Which,
          executor: Executor) -> None:
    python = _parsing_python(heads)
    if not python:
        return stage.skip("no changed Python file that compiles")
    ruff = which("ruff")
    if ruff is None:
        return stage.skip("ruff is not installed")
    code, output = executor([ruff, "check", "--isolated", "--no-cache", "--select", RUFF_RULES, "--output-format", "json",
                             "--", *python], workdir, testing.sandbox_env(), TOOL_TIMEOUT_S)
    if code not in (0, 1):
        return stage.fail(f"ruff exited {code}: {_text(output, 160)}")
    try:
        reported = json.loads(output[output.index("["):]) if "[" in output else []
    except ValueError:
        return stage.fail(f"ruff's output was not JSON: {_text(output, 160)}")
    stage.files = python
    for item in reported:
        path = os.path.relpath(str(item.get("filename") or ""), workdir) if os.path.isabs(str(item.get("filename") or "")) \
            else str(item.get("filename") or "")
        line = int((item.get("location") or {}).get("row") or 0)
        stage.flagged.add(path)
        if item.get("code") not in RUFF_RULES.split(",") or line not in added.get(path, set()):
            continue
        message = f"{item.get('code')}: {item.get('message')}"
        stage.add(_finding("lint", path, line, f"{_text(item.get('message'), 160)} ({item.get('code')}).", message,
                           level="high", category="correctness", tool="ruff"))


def _typecheck(stage: _Stage, heads: dict[str, str], added: dict[str, set[int]], workdir: str, which: Which,
               executor: Executor) -> None:
    python = _parsing_python(heads)
    if not python:
        return stage.skip("no changed Python file that compiles")
    mypy = which("mypy")
    if mypy is None:
        return stage.skip("mypy is not installed")
    code, output = executor([mypy, "--config-file=", "--ignore-missing-imports", "--follow-imports=skip",
                             "--no-incremental", "--cache-dir=/dev/null", "--no-error-summary", "--show-error-codes",
                             "--", *python], workdir, testing.sandbox_env(), TOOL_TIMEOUT_S)
    if code not in (0, 1):
        return stage.fail(f"mypy exited {code}: {_text(output, 160)}")
    stage.files = python
    for match in _MYPY_LINE.finditer(output):
        path, line = match.group("path").strip(), int(match.group("line"))
        stage.flagged.add(path)
        if line not in added.get(path, set()):
            continue
        message = match.group("message") + (f" [{match.group('code')}]" if match.group("code") else "")
        stage.add(_finding("typecheck", path, line, f"Type error: {_text(match.group('message'), 160)}.", message,
                           level="medium", category="correctness", tool="mypy"))


async def _loki(stage: _Stage, rlm: Any, heads: dict[str, str], bases: dict[str, str | None],
                added: dict[str, set[int]]) -> None:
    request = getattr(rlm, "host_request", None)
    if request is None:
        return stage.skip("the host serves no Loki scan")
    changes = [[path, bases.get(path) or "", text] for path, text in heads.items()]
    try:
        reply = await request("review.loki", {"changes": changes})
    except Exception as error:  # the host may not serve it (an older engine): the review goes on without it
        return stage.skip(f"the Loki scan is not available ({_text(str(error), 120)})")
    stage.files = list(heads)
    for violation in (reply or {}).get("violations") or []:
        match = _LOKI_LINE.match(str(violation))
        if match is None:
            continue
        path, line, rule = match.group("path"), int(match.group("line")), match.group("rule")
        stage.flagged.add(path)
        if line not in added.get(path, set()):
            continue
        security = rule in ("loki/secret", "loki/actions-injection")
        stage.add(_finding("loki", path, line, f"{_text(match.group('message'), 200)} ({rule}).", str(violation),
                           level="high" if security else "medium", category="security" if security else "correctness",
                           tool=f"Loki ({rule})"))


async def run_static(rlm: Any, files: list[FileDiff], read_head: Callable[[str], list[str] | None],
                     read_base: Callable[[str], list[str] | None], *, which: Which | None = None,
                     executor: Executor | None = None) -> dict[str, Any]:
    """Run every stage over the changed files (not deleted, not binary). Returns {"stages": [{name, status: clean |
    findings | skipped | failed, note, files, findings, ms}], "findings": [host-confirmed findings], "clean":
    {path: [stages that checked it and found nothing]}}. `which` and `executor` find and run the tools (default:
    the PATH, and a process of their own with the sandbox's bare environment)."""
    which = which or shutil.which
    executor = executor or testing.run_process
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
        bases[item.path] = None if item.status == "added" else _text_of(read_base(item.old_path or item.path))
        added[item.path] = _new_lines(item)
    stages = {name: _Stage(name) for name in STAGES}
    if not heads:
        for stage in stages.values():
            stage.skip("no changed file could be read")
        return {"stages": [stage.record() for stage in stages.values()], "findings": [], "clean": {}}
    workdir = tempfile.mkdtemp(prefix="ultron-autoreview-static-")
    try:
        for path, text in heads.items():
            target = os.path.join(workdir, path)
            if not os.path.realpath(target).startswith(os.path.realpath(workdir) + os.sep):
                continue
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with open(target, "w", encoding="utf-8") as handle:
                handle.write(text)
        for name, run in (("compile", lambda s: _compile(s, heads, bases, workdir, which, executor)),
                          ("lint", lambda s: _lint(s, heads, added, workdir, which, executor)),
                          ("typecheck", lambda s: _typecheck(s, heads, added, workdir, which, executor))):
            started = time.monotonic()
            try:
                run(stages[name])
            except Exception as error:  # one stage failing leaves the others standing
                stages[name].fail(error)
            stages[name].ms = int((time.monotonic() - started) * 1000)
        started = time.monotonic()
        try:
            await _loki(stages["loki"], rlm, heads, bases, added)
        except Exception as error:
            stages["loki"].fail(error)
        stages["loki"].ms = int((time.monotonic() - started) * 1000)
    finally:
        shutil.rmtree(workdir, ignore_errors=True)
    findings = [finding for stage in stages.values() if stage.status == "clean" for finding in stage.findings]
    clean: dict[str, list[str]] = {}
    for stage in stages.values():
        if stage.status != "clean":
            continue
        for path in stage.files:
            if path not in stage.flagged:
                clean.setdefault(path, []).append(stage.name)
    return {"stages": [stage.record() for stage in stages.values()], "findings": findings, "clean": clean}


def contradiction(finding: dict[str, Any], static: dict[str, Any] | None) -> str | None:
    """Why the deterministic stage contradicts a model's finding, or None: its claim is one a stage settles, and
    that stage checked the file and found nothing."""
    if not static:
        return None
    clean = static["clean"].get(finding.get("file") or "") or []
    text = f"{finding.get('claim') or ''} {finding.get('why') or ''}"
    for stage, pattern in CONTRADICTED_BY:
        if stage in clean and pattern.search(text):
            return f"{stage} ran clean on {finding['file']}"
    return None


def notes(static: dict[str, Any]) -> list[str]:
    """`notChecked` lines for stages that failed (a skipped stage is reported in `static.stages` only)."""
    return [f"The {stage['name']} check of the deterministic stage failed ({stage['note']})."
            for stage in static["stages"] if stage["status"] == "failed"]
