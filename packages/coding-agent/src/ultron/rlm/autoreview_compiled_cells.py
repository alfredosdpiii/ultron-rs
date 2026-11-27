"""Sandboxed REPL cell runner and `rv` session for compiled autoreview planning."""
from __future__ import annotations

import asyncio
import json
import shutil
import tempfile
from typing import Any, Callable

import autoreview_deep as deep
import autoreview_tests as testing
from autoreview_compiled_exec import Interpreter
from autoreview_compiled_plan import (
    _ID,
    CELL_OUTPUT_CHARS,
    CELL_TIMEOUT_S,
    CELL_TRANSCRIPT_CHARS,
    CELL_TRANSCRIPT_FULL,
    CELL_VALUE_CHARS,
    CELL_VALUE_ITEMS,
    LOOKUPS,
    MAX_ASKS,
    TEST_OPS,
    _check_step,
    _normalize_step,
    as_dict,
    as_list,
    uncovered_items,
    validate,
)
from infer_api import FrameError, Incomplete
from review_api import _clip, _text
from review_prompts import RV_API

CELL_RUNNER = r'''
import sys, json, io, traceback, builtins
_in, _out = sys.stdin, sys.stdout
PROBE = sys.argv[1:] == ["--probe"]
def _send(obj):
    _out.write(json.dumps(obj, default=str) + "\n"); _out.flush()
def _recv():
    line = _in.readline()
    if not line:
        raise SystemExit(0)
    return json.loads(line)
class RvError(Exception):
    pass
class _Rv:
    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        def call(*args, **kwargs):
            _send({"call": name, "args": list(args), "kwargs": kwargs})
            reply = _recv()
            if "error" in reply:
                raise RvError(reply["error"])
            return reply.get("value")
        call.__name__ = name
        return call
rv = _Rv()
_ALLOWED = {
    "re", "json", "math", "collections", "itertools", "functools",
    "textwrap", "string", "operator", "statistics", "difflib",
}
_real_import = builtins.__import__
def _import(name, globals=None, locals=None, fromlist=(), level=0):
    if level == 0 and name.split(".")[0] in _ALLOWED:
        return _real_import(name, globals, locals, fromlist, level)
    msg = (
        "import of %r is not available in a planner cell: "
        "the repository is reached through rv"
    )
    raise ImportError(msg % name)
_SAFE_NAMES = (
    "abs", "all", "any", "bool", "chr", "dict", "divmod", "enumerate",
    "filter", "float", "format", "frozenset", "hash", "int", "isinstance",
    "issubclass", "iter", "len", "list", "map", "max", "min", "next", "ord",
    "print", "range", "repr", "reversed", "round", "set", "slice", "sorted",
    "str", "sum", "tuple", "zip", "Exception", "ValueError", "TypeError",
    "KeyError", "IndexError", "StopIteration", "AttributeError",
    "RuntimeError", "ZeroDivisionError", "NameError", "ImportError",
    "AssertionError", "LookupError", "ArithmeticError",
    "NotImplementedError", "__build_class__",
)
_SAFE = {
    name: getattr(builtins, name)
    for name in _SAFE_NAMES
    if hasattr(builtins, name)
}
_SAFE["__import__"] = _import
_globals = {
    "__builtins__": builtins if PROBE else _SAFE,
    "rv": rv,
    "RvError": RvError,
    "__name__": "__cell__",
}
while True:
    message = _recv()
    code = message.get("cell") or ""
    buffer = io.StringIO()
    error = ""
    previous = sys.stdout
    sys.stdout = buffer
    try:
        exec(compile(code, "<cell>", "exec"), _globals)
    except SystemExit:
        error = "SystemExit is not available in a planner cell: call rv.done() instead"
    except BaseException as exc:
        tail = traceback.extract_tb(sys.exc_info()[2])[-1:]
        in_cell = bool(tail and tail[0].filename == "<cell>")
        where = " (line %d of the cell)" % tail[0].lineno if in_cell else ""
        error = "".join(traceback.format_exception_only(type(exc), exc)).strip() + where
    finally:
        sys.stdout = previous
    _send({"done": True, "output": buffer.getvalue()[-8000:], "error": error})
'''

CELL_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {"cell": {"type": "string"}, "done": {"type": "boolean"}},
    "required": ["cell", "done"],
}

_OP_POSITIONAL_ARGS: dict[str, tuple[str, ...]] = {
    "read": ("path", "start", "end"),
    "read_diff": ("path", "start", "end"),
    "grep": ("pattern", "glob", "start", "end", "count_only", "max"),
    "list": ("dir",),
    "definition": ("symbol",),
    "references": ("symbol",),
    "history": ("path", "n"),
    "blame_range": ("path", "start", "end"),
    "pickaxe": ("string", "n"),
    "run_tests": ("paths", "select"),
    "mutation_check": ("path", "line", "replacement", "tests"),
}

_FINDING_ARG_NAMES = (
    "when",
    "file",
    "line",
    "level",
    "category",
    "claim",
    "why",
    "fix",
    "scenario",
    "evidence",
)

_FINDING_RAW_KEYS = (
    "file",
    "line",
    "level",
    "category",
    "claim",
    "why",
    "fix",
    "scenario",
    "evidence",
    "citations",
    "unpinned",
    "consequence",
)


class RvError(Exception):
    """An rv call the host refuses; the message is raised inside the cell as RvError."""


class CellRunner:
    """A sandboxed Python child that runs the planner's cells and relays its `rv` calls."""

    def __init__(self, sandbox: Any, *, python: str = "python3", probe: bool = False) -> None:
        self.sandbox = sandbox
        self.python = python
        self.probe = probe
        self.process: Any = None
        self.workdir: str | None = None

    async def start(self) -> None:
        self.workdir = tempfile.mkdtemp(prefix="ultron-autoreview-cell-")
        command = [self.python, "-I", "-c", CELL_RUNNER] + (["--probe"] if self.probe else [])
        argv = (
            self.sandbox.wrap(command, self.workdir, testing.sandbox_env())
            if self.sandbox is not None
            else command
        )
        self.process = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            cwd=self.workdir,
            env=testing._launch_env(),
        )

    async def _read_stderr_on_exit(self) -> str:
        stderr = b""
        if self.process is not None and self.process.stderr is not None:
            try:
                stderr = await asyncio.wait_for(self.process.stderr.read(), 2)
            except asyncio.TimeoutError:
                stderr = b""
        await self.close()
        detail = _text(stderr.decode("utf-8", "replace"), 300)
        return "the planner's sandbox exited: " + detail

    async def _handle_cell_call(
        self,
        message: dict[str, Any],
        handler: Callable[[str, list[Any], dict[str, Any]], Any],
    ) -> None:
        call_name = str(message["call"])
        call_args = list(as_list(message.get("args")))
        call_kwargs = dict(as_dict(message.get("kwargs")))
        try:
            value = await handler(call_name, call_args, call_kwargs)
            reply: dict[str, Any] = {"value": value}
        except RvError as error:
            reply = {"error": str(error)}
        except Exception as error:
            reply = {"error": f"host error: {type(error).__name__}: {_text(str(error), 200)}"}
        self.process.stdin.write((json.dumps(reply, default=str) + "\n").encode("utf-8"))
        await self.process.stdin.drain()

    async def _read_cell_message(self, timeout_s: float) -> tuple[dict[str, Any] | None, str | None]:
        try:
            line = await asyncio.wait_for(self.process.stdout.readline(), timeout_s)
        except asyncio.TimeoutError:
            await self.close()
            msg = (
                f"the cell did not finish within {timeout_s:.0f} s "
                "and the planner's sandbox was closed"
            )
            return None, msg
        if not line:
            return None, await self._read_stderr_on_exit()
        try:
            return json.loads(line.decode("utf-8", "replace")), None
        except ValueError:
            return {}, None

    def _require_started(self) -> None:
        if self.process is None or self.process.stdin is None or self.process.stdout is None:
            raise RuntimeError("CellRunner process is not started")

    @staticmethod
    def _extract_done_result(message: dict[str, Any]) -> tuple[str, str]:
        raw_output = message.get("output")
        output = str(raw_output if raw_output else "")[-CELL_OUTPUT_CHARS:]
        raw_error = message.get("error")
        return output, str(raw_error if raw_error else "")

    async def run_cell(
        self,
        code: str,
        handler: Callable[[str, list[Any], dict[str, Any]], Any],
        timeout_s: float = CELL_TIMEOUT_S,
    ) -> tuple[str, str]:
        """Run one cell; `handler(name, args, kwargs)` answers each rv call."""
        self._require_started()
        self.process.stdin.write((json.dumps({"cell": code}) + "\n").encode("utf-8"))
        await self.process.stdin.drain()

        while True:
            message, failure = await self._read_cell_message(timeout_s)
            if failure is not None:
                return "", failure
            if not message:
                continue
            if message.get("done"):
                return self._extract_done_result(message)
            if "call" in message:
                await self._handle_cell_call(message, handler)

    async def close(self) -> None:
        process, self.process = self.process, None
        if process is not None:
            try:
                process.kill()
            except ProcessLookupError:
                pass
            try:
                await asyncio.wait_for(process.wait(), 5)
            except (asyncio.TimeoutError, ProcessLookupError):
                pass
        if self.workdir:
            shutil.rmtree(self.workdir, ignore_errors=True)
            self.workdir = None


def _public_value(result: dict[str, Any]) -> dict[str, Any]:
    """What a cell gets back from an rv call: the step's result, bounded."""
    out: dict[str, Any] = {"id": result.get("id"), "status": result.get("status")}
    for key in (
        "count",
        "capped",
        "truncated",
        "answer",
        "quote",
        "why",
        "value",
        "contradicted",
        "detail",
        "emitted",
        "caught",
        "title",
    ):
        if key in result and result[key] is not None:
            out[key] = result[key]
    if result.get("op") in TEST_OPS:
        out["status"] = result.get("outcome")
    if result.get("items"):
        out["items"] = result["items"][:CELL_VALUE_ITEMS]
    if result.get("text"):
        out["text"] = _clip(result["text"], CELL_VALUE_CHARS)
    if result.get("op") == "finding":
        emitted_text = "finding emitted" if result.get("emitted") else ""
        out["gate"] = result.get("gate") or emitted_text
    return out


class CellSession:
    """The `rv` API as the host serves it on top of the shared Interpreter."""

    def __init__(
        self,
        interpreter: Interpreter,
        coverage: list[dict[str, str]],
        limits: tuple[int, int],
    ) -> None:
        self.interpreter = interpreter
        self.coverage = coverage
        self.limits = limits
        self.covered: set[str] = set()
        self.declared: list[str] = []
        self.done = False
        self.counter = 0
        self.cells: list[str] = []
        self.cells_left = 0
        self.materialised = 0

    def _id(self, given: Any) -> str:
        if given is not None:
            if not isinstance(given, str) or not _ID.match(given):
                raise RvError(f"id must match {_ID.pattern}")
            if given in self.interpreter.steps_by_id:
                raise RvError(f"id {given!r} is already used")
            return given
        while True:
            self.counter += 1
            sid = f"s{self.counter}"
            if sid not in self.interpreter.steps_by_id:
                return sid

    def _check_step_budget(self) -> None:
        total = len(self.interpreter.steps_by_id)
        written = sum(1 for sid in self.interpreter.steps_by_id if "[" not in sid)
        if total >= self.limits[1] or written >= self.limits[0]:
            raise RvError(
                f"the step limit is reached ({self.limits[0]} steps as written, "
                f"{self.limits[1]} after expansion)"
            )

    def _step(self, raw: dict[str, Any]) -> dict[str, Any]:
        """Validate one synthesized step against the language and the steps so far."""
        self._check_step_budget()
        errors: list[str] = []
        step = _normalize_step(raw, errors)
        if step is not None:
            by_id = dict(self.interpreter.steps_by_id)
            by_id[step["id"]] = step
            _check_step(step, by_id, set(by_id), errors)
            if step["op"] == "ask" and self.interpreter.asks >= MAX_ASKS:
                errors.append(f"the limit of {MAX_ASKS} asks is reached")
        if errors or step is None:
            raise RvError("; ".join(errors) or "invalid step")
        return step

    def _call_done(self) -> dict[str, Any]:
        missing = uncovered_items(
            self.coverage,
            list(self.interpreter.steps_by_id.values()),
            self.declared + [f"{item}:" for item in self.covered],
        )
        if missing:
            return {
                "ok": False,
                "uncovered": [f"{item['id']}: {item['text']}" for item in missing],
                "hint": (
                    "cover each with a check (covers=[id]) or rv.uncovered(id, why), "
                    "then call rv.done() again"
                ),
                "cells_left": self.cells_left,
            }

        orphans = self.decided_without_finding()
        if orphans:
            return {
                "ok": False,
                "undecided_findings": orphans,
                "hint": (
                    "these asks answered yes but no finding rests on them: attach one with "
                    "rv.finding(when=<id>, ...) or state why there is nothing to report with "
                    "rv.uncovered-style text in a print, then call rv.done() again"
                ),
                "cells_left": self.cells_left,
            }
        self.done = True
        return {"ok": True}

    def _call_cover(self, args: list[Any], kwargs: dict[str, Any]) -> dict[str, Any]:
        item = str(args[0] if args else kwargs.get("item", ""))
        if item not in {entry["id"] for entry in self.coverage}:
            raise RvError(f"{item!r} is not a coverage item")
        self.covered.add(item)
        return {"ok": True}

    def _call_uncovered(self, args: list[Any], kwargs: dict[str, Any]) -> dict[str, Any]:
        item = str(args[0] if args else kwargs.get("item", ""))
        why = str(args[1] if len(args) > 1 else kwargs.get("why", ""))
        if item not in {entry["id"] for entry in self.coverage}:
            raise RvError(f"{item!r} is not a coverage item")
        self.declared.append(f"{item}: {why}")
        return {"ok": True}

    async def _call_run_program(self, args: list[Any], kwargs: dict[str, Any]) -> dict[str, Any]:
        program = args[0] if args else kwargs.get("program")
        validated, errors = validate(program, None, self.limits[0])
        if validated is None:
            raise RvError("the program is invalid: " + "; ".join(errors[:8]))
        clash = [
            step["id"]
            for step in validated.steps
            if step["id"] in self.interpreter.steps_by_id
        ]
        if clash:
            raise RvError("these ids are already used: " + ", ".join(clash))
        self.declared += validated.uncovered
        await self.interpreter.run(validated.steps)
        return {
            "ok": True,
            "steps": [
                _public_value(dict(self.interpreter.results[step["id"]], id=step["id"]))
                for step in validated.steps
                if step["id"] in self.interpreter.results
            ],
        }

    async def _run_attached_finding(
        self,
        step_id: str,
        attached: dict[str, Any],
    ) -> dict[str, Any]:
        spec = dict(attached)
        negate = bool(spec.pop("not_", False) or spec.pop("negate", False))
        spec.pop("when", None)
        finding_raw = self._raw("finding", [], {"when": step_id, "not_": negate, **spec})
        finding_step = self._step(finding_raw)
        finding_result = await self.interpreter.execute(finding_step)
        return _public_value(dict(finding_result, id=finding_step["id"]))

    async def _dispatch_control(
        self,
        name: str,
        args: list[Any],
        kwargs: dict[str, Any],
    ) -> tuple[bool, Any]:
        if name == "help":
            return True, RV_API
        if name == "done":
            return True, self._call_done()
        if name == "cover":
            return True, self._call_cover(args, kwargs)
        if name == "uncovered":
            return True, self._call_uncovered(args, kwargs)
        if name == "run_program":
            return True, await self._call_run_program(args, kwargs)
        return False, None

    async def call(self, name: str, args: list[Any], kwargs: dict[str, Any]) -> Any:
        if self.done:
            raise RvError("rv.done() was already called")

        handled, control_result = await self._dispatch_control(name, args, kwargs)
        if handled:
            return control_result

        attached = kwargs.pop("finding", None) if name in ("ask", "assert_", "assert") else None
        if attached is not None and not isinstance(attached, dict):
            raise RvError(
                "finding= takes a dict with the finding's fields "
                "(file, line, level, category, claim, ...)"
            )

        step = self._step(self._raw(name, args, kwargs))
        result = await self.interpreter.execute(step)
        value = _public_value(dict(result, id=step["id"]))
        if attached is not None:
            value["finding"] = await self._run_attached_finding(step["id"], attached)
        value["cells_left"] = self.cells_left
        return value

    def decided_without_finding(self) -> list[str]:
        """Asks answered yes that no finding step rests on."""
        used: set[str] = set()
        for step in self.interpreter.steps_by_id.values():
            if step["op"] == "finding":
                used.add(step["when"]["step"])
                used.update(as_list(step.get("evidence")))
        return [
            sid
            for sid, result in self.interpreter.results.items()
            if result.get("op") == "ask"
            and result.get("status") == "ok"
            and result.get("answer") == "yes"
            and sid not in used
        ]

    def _place_from_read(self, source: dict[str, Any]) -> tuple[str, int] | None:
        args = as_dict(source.get("args"))
        if source.get("op") != "read" or not isinstance(args.get("path"), str):
            return None
        try:
            return self.interpreter.repo.path(args["path"]), max(1, int(args.get("start") or 1))
        except (deep.Rejected, TypeError, ValueError):
            return None

    def _place_from_items(self, result: dict[str, Any]) -> tuple[str, int] | None:
        for item in as_list(result.get("items")):
            if not isinstance(item, dict):
                continue
            path_val, line_val = item.get("path"), item.get("line")
            if not isinstance(path_val, str) or not isinstance(line_val, int):
                continue
            try:
                return self.interpreter.repo.path(path_val), line_val
            except deep.Rejected:
                continue
        return None

    def _place(self, ask_id: str) -> tuple[str, int] | None:
        """A file and line for a finding materialised from an ask."""
        step = as_dict(self.interpreter.steps_by_id.get(ask_id))
        for ref in as_list(step.get("context")):
            located = self._place_from_read(as_dict(self.interpreter.steps_by_id.get(ref)))
            if located is not None:
                return located
            located = self._place_from_items(as_dict(self.interpreter.results.get(ref)))
            if located is not None:
                return located
        return None

    async def materialise(self) -> list[str]:
        """Turn remaining yes-answered asks without findings into resolved candidates."""
        notes: list[str] = []
        for ask_id in self.decided_without_finding():
            ask = self.interpreter.steps_by_id[ask_id]
            result = self.interpreter.results[ask_id]
            place = self._place(ask_id)
            if place is None:
                notes.append(
                    f"ask {ask_id} was answered yes but no finding rests on it "
                    "and it names no file to place one at"
                )
                continue
            path, line = place
            raw = {
                "id": f"{ask_id}_f",
                "op": "finding",
                "when": {"step": ask_id},
                "file": path,
                "line": line,
                "level": "medium",
                "category": "correctness",
                "claim": _text(ask["question"], 300),
                "why": _text(f"The small model answered yes: {result.get('why') or ''}", 600),
                "scenario": "",
                "evidence": [ask_id, *as_list(ask.get("context"))],
                "citations": [],
            }
            errors: list[str] = []
            step = _normalize_step(raw, errors)
            if step is None or errors:
                notes.append(
                    f"ask {ask_id} could not be materialised as a finding: "
                    + "; ".join(errors[:2])
                )
                continue
            self.interpreter.steps_by_id[step["id"]] = step
            reason = "materialised from an ask answered yes that the planner left without a finding"
            await self.interpreter._resolve([(step, {}, reason)])
            self.materialised += 1
        return notes

    def _raw_lookup_or_test(
        self,
        sid: str,
        name: str,
        args: list[Any],
        kwargs: dict[str, Any],
        covers: Any,
    ) -> dict[str, Any]:
        params = dict(zip(_OP_POSITIONAL_ARGS[name], args))
        params.update(kwargs)
        if "glob" in params:
            params["path_glob"] = params.pop("glob")
        cleaned = {key: value for key, value in params.items() if value is not None}
        return {"id": sid, "op": name, "args": cleaned, "covers": covers}

    def _raw_finding(
        self,
        sid: str,
        args: list[Any],
        kwargs: dict[str, Any],
        covers: Any,
    ) -> dict[str, Any]:
        params = dict(zip(_FINDING_ARG_NAMES, args))
        params.update(kwargs)
        when = params.get("when")
        if isinstance(when, str):
            when = {"step": when, "not": bool(params.get("not_") or params.get("negate"))}
        return {
            "id": sid,
            "op": "finding",
            "when": when,
            "covers": covers,
            **{key: params.get(key) for key in _FINDING_RAW_KEYS},
        }

    def _raw(self, name: str, args: list[Any], kwargs: dict[str, Any]) -> dict[str, Any]:
        """One rv call as the JSON step it stands for."""
        kwargs = dict(kwargs)
        sid = self._id(kwargs.pop("id", None))
        covers = kwargs.pop("covers", None)

        if name in LOOKUPS or name in TEST_OPS:
            return self._raw_lookup_or_test(sid, name, args, kwargs, covers)
        if name == "ask":
            params = dict(zip(("question", "context"), args))
            params.update(kwargs)
            return {
                "id": sid,
                "op": "ask",
                "question": params.get("question"),
                "context": params.get("context"),
                "covers": covers,
            }
        if name in ("assert_", "assert"):
            params = dict(zip(("step", "predicate", "expect", "holds"), args))
            params.update(kwargs)
            return {
                "id": sid,
                "op": "assert",
                "step": params.get("step"),
                "predicate": params.get("predicate"),
                "expect": params.get("expect"),
                "holds": params.get("holds") or "",
                "all": params.get("all"),
                "any": params.get("any"),
                "covers": covers,
            }
        if name == "finding":
            return self._raw_finding(sid, args, kwargs, covers)
        raise RvError(f"rv has no method {name!r}; rv.help() lists them")


def _plan_tokens(frames: Any) -> int:
    return sum(
        int(item.get("tokens") or 0) for item in frames.timings if item.get("phase") == "plan"
    )


def _shown_transcript(transcript: list[str]) -> str:
    parts: list[str] = []
    full_start = len(transcript) - CELL_TRANSCRIPT_FULL
    for index, entry in enumerate(transcript):
        if index >= full_start:
            parts.append(entry)
            continue
        first = next((line for line in entry.split("\n")[1:] if line.strip()), "")
        calls = entry.count("rv.")
        err_note = " with an error" if "\nError: " in entry else ""
        header = entry.split("\n", 1)[0]
        clipped = _clip(first.strip(), 100)
        parts.append(f"{header} (summary) {clipped} ... {calls} rv call(s){err_note}")
    return "\n\n".join(parts)[-CELL_TRANSCRIPT_CHARS:]


def _cell_turn_extra(number: int, cells: int, transcript: list[str]) -> list[str]:
    last = number == cells
    shown = _shown_transcript(transcript) or "(none yet)"
    last_suffix = ", the last" if last else ""
    extra = [
        f"Cells so far and their output (cell {number} of at most {cells}; "
        f"{cells - number} left after this one{last_suffix}):\n{shown}"
    ]
    if last:
        extra.append(
            "This is your last cell: emit a finding for every decided check that supports one "
            "(rv.finding, or finding= on the check) and call rv.done(). Asks answered yes "
            "without a finding are put to the small model as candidates by the host, but with "
            "your words they would be better."
        )
    return extra


def _reply_failure_reason(reply: Any) -> str:
    error = getattr(reply, "error", None)
    if error:
        return str(error)
    status = getattr(reply, "status", None)
    return str(status) if status else "no reply"


async def _execute_cell_code(
    number: int,
    code: str,
    reply_done: bool,
    session: CellSession,
    runner: CellRunner,
    transcript: list[str],
    notes: list[str],
) -> tuple[bool, str]:
    session.cells.append(code)
    output, error = await runner.run_cell(code, session.call)
    err_line = f"\nError: {error}" if error else ""
    shown_output = output or "(no output)"
    transcript.append(f"Cell {number}:\n{code}\nOutput:\n{shown_output}{err_line}")

    if runner.process is None:
        notes.append(error or "the planner's sandbox exited")
        return True, "failed"
    return bool(session.done or reply_done), "ok"


async def _run_cell_turn(
    number: int,
    cells: int,
    *,
    frames: Any,
    views: list[str],
    task: str,
    plan_model: str | None,
    plan_thinking: str | None,
    cutoff: float | None,
    session: CellSession,
    runner: CellRunner,
    transcript: list[str],
    notes: list[str],
    cell_tokens: list[int],
) -> tuple[bool, str]:
    session.cells_left = cells - number
    extra = _cell_turn_extra(number, cells, transcript)
    before = _plan_tokens(frames)
    replies = await frames.run(
        "plan",
        [("planner", task, views + extra)],
        contract=CELL_CONTRACT,
        model=plan_model,
        thinking=plan_thinking,
        cutoff=cutoff,
    )
    cell_tokens.append(_plan_tokens(frames) - before)
    reply = replies[0]

    if isinstance(reply, (Incomplete, FrameError)) or not isinstance(reply, dict):
        why = _reply_failure_reason(reply)
        notes.append(f"the planner frame failed at cell {number} ({_text(why, 160)})")
        return True, "failed"

    code = reply.get("cell") if isinstance(reply.get("cell"), str) else ""
    if not code.strip():
        transcript.append(f"Cell {number}: (empty)")
        return reply.get("done") is True, "ok"

    return await _execute_cell_code(
        number, code, reply.get("done") is True, session, runner, transcript, notes
    )


async def run_cells(
    frames: Any,
    *,
    views: list[str],
    task: str,
    plan_model: str | None,
    plan_thinking: str | None,
    cutoff: float | None,
    clock: Callable[[], float],
    interpreter: Interpreter,
    coverage: list[dict[str, str]],
    limits: tuple[int, int],
    sandbox: Any,
    cells: int,
    python: str = "python3",
) -> dict[str, Any]:
    """The planner as a REPL: up to `cells` frames, each returning one Python cell."""
    session = CellSession(interpreter, coverage, limits)
    runner = CellRunner(sandbox, python=python)
    transcript: list[str] = []
    notes: list[str] = []
    began = clock()
    status = "ok"
    cell_tokens: list[int] = []

    try:
        await runner.start()
    except Exception as error:
        return {
            "session": session,
            "status": "failed",
            "error": f"the planner's sandbox could not start: {_text(str(error), 200)}",
            "notes": notes,
            "ms": int((clock() - began) * 1000),
        }

    try:
        for number in range(1, cells + 1):
            should_stop, status = await _run_cell_turn(
                number,
                cells,
                frames=frames,
                views=views,
                task=task,
                plan_model=plan_model,
                plan_thinking=plan_thinking,
                cutoff=cutoff,
                session=session,
                runner=runner,
                transcript=transcript,
                notes=notes,
                cell_tokens=cell_tokens,
            )
            if should_stop:
                break
        if not session.done and status == "ok":
            notes.append(f"the planner did not call rv.done() within {cells} cells")
        notes.extend(await session.materialise())
    finally:
        await runner.close()

    return {
        "session": session,
        "status": status,
        "notes": notes,
        "ms": int((clock() - began) * 1000),
        "transcript": transcript,
        "cell_tokens": cell_tokens,
    }
