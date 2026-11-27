from __future__ import annotations

import ast
import asyncio
import contextlib
import contextvars
import hashlib
import io
import json
import math
import os
import re
import signal
import sys
import threading
import time
import tokenize
import weakref
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from agents_api import Agents, Workflows
from budget_api import BudgetEstimator
from memory_api import Memory, Refinements
from family_api import AgentMessages
from progress_api import Progress
from skills_api import Skills
from schedules_api import Goals, Schedules
from instances_api import Instances
from lanes_api import child_selectors, install as install_lanes
from release_gate_api import ReleaseGates
import agent_class_api
from context_api import Context
from ext_api import install as install_extensions
from infer_api import ContextHandle, install as install_inference
from hints_api import Hints
from goal_api import Goal
from secret_patterns import REDACTION_MARKER
from swarm_api import install as install_swarm
from images_api import install as install_images
from schedule_api import Schedule
from diagram_api import Diagram
from sessions_api import Sessions
from judge_api import Judge
from knowledge_api import KnowledgeApi
import waits
from tools_api import Mcp, McpError, ToolCall, ToolError, ToolResult, Tools
from worktree_api import Worktrees, worktree_path
from runtime_console import (
    ConsoleBuffer,
    ReplGlobalOverwrite as ReplGlobalOverwrite,
    _CORE_HOST_BINDINGS,
    _ConsoleTee,
    _check_protected_overwrites,
)
from runtime_preview import (
    _BoundedTextIO as _BoundedTextIO,
    _MARKER_BYTES as _MARKER_BYTES,
    _PREVIEW_BYTES as _PREVIEW_BYTES,
    _TRUNCATION_MARKER as _TRUNCATION_MARKER,
    _cell_result_text,
    _exception_message,
    _frame_preview as _frame_preview,
    _preview_deadline,
    _reference_preview as _reference_preview,
    _render_preview as _render_preview,
    _result_preview,
    _safe_repr as _safe_repr,
    _text_preview,
    _type_name as _type_name,
    preview,
    render_error_preview,
)
from runtime_snapshot import (
    SnapshotIntegrityError as SnapshotIntegrityError,
    _NonRestorable as _NonRestorable,
    _checked_definitions as _checked_definitions,
    _decode_value as _decode_value,
    _encode_value as _encode_value,
    _persistable_namespace as _snapshot_persistable,
    restore_snapshot_file,
    save_snapshot_file,
)

@dataclass
class SpawnHandle:
    rlm_child_id: str
    name: str
    session_dir: str
    model: str
    timeout_ms: int
    parent_branch_anchor: str
    # `worktree=True`: {"branch", "path", "cwd"} of the child's private worktree, else None.
    worktree: dict[str, Any] | None = None

    @property
    def id(self) -> str:
        """The child's id (``rlm_child_id``), as ``agents.result(id)`` and runtime events name it."""
        return self.rlm_child_id

    def __repr__(self) -> str:
        branch = f", branch={self.worktree['branch']!r}" if self.worktree else ""
        return f"SpawnHandle(name={self.name!r}, rlm_child_id={self.rlm_child_id!r}{branch})"


# json.dumps escapes non-ASCII, so a line's length is its byte count; kept under the host's MAX_FRAME_BYTES.
_MAX_HOST_FRAME_BYTES = 1024 * 1024 - 1024


# The execute request whose code is running in this context. Set by execute_cell; tasks and `asyncio.to_thread` calls a
# cell starts inherit it, so their host requests and prints stay the cell's own after a later cell has started.
_CELL: contextvars.ContextVar[str | None] = contextvars.ContextVar("ultron_rlm_cell", default=None)


class HostBridge:
    """Host requests: a request is registered, written (with its cell), then answered by the host's reply. The host
    answers every request while the kernel lives; a request the caller stops awaiting is cancelled on the host
    (`host_cancel`), and its late reply is dropped here."""

    def __init__(self) -> None:
        self._host_requests: dict[str, asyncio.Future[Any]] = {}
        self._counter = 0

    async def request(self, request_type: str, payload: dict[str, Any] | None = None) -> Any:
        self._counter += 1
        request_id = f"host-{self._counter}"
        frame: dict[str, Any] = {"event": "host_request", "id": request_id, "type": request_type, "payload": payload or {}}
        cell = _CELL.get()
        if cell is not None:
            frame["cell"] = cell
        line = json.dumps(frame, separators=(",", ":")) + "\n"
        # The host kills a kernel that writes a frame over 1 MiB, losing every variable: refuse it here instead.
        if len(line) > _MAX_HOST_FRAME_BYTES:
            raise ValueError(f"{request_type} request is {len(line):,} bytes, over the kernel's 1 MiB frame: pass "
                             "rlm.load handles or file paths instead of inline text, or send it in parts")
        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self._host_requests[request_id] = future
        try:
            _write_frame(line)
            return await future
        finally:
            self._host_requests.pop(request_id, None)
            if future.cancelled():
                # The awaiting task was cancelled (asyncio.wait_for, a cancelled gather): stop the host's work too.
                try:
                    emit("host_cancel", id=request_id)
                except Exception:
                    pass

    def resolve(self, request_id: str, payload: Any = None, error: str | None = None) -> None:
        future = self._host_requests.get(request_id)
        if future is None or future.done():
            return
        if error is not None:
            future.set_exception(RuntimeError(error))
        else:
            future.set_result(payload)


_SPAWN_ALLOWED_OPTIONS = frozenset({
    "name",
    "model",
    "thinking",
    "timeout_ms",
    "depth",
    "worktree",
    "worktree_setup",
    "cwd",
    "context",
    "fork",
})


def _validate_spawn_core(prompt: Any, kwargs: dict[str, Any]) -> None:
    if not isinstance(prompt, str) or not prompt.strip():
        raise ValueError("rlm.spawn prompt must be a non-empty string")
    name = kwargs.get("name")
    if not isinstance(name, str) or not name.strip():
        raise ValueError("rlm.spawn name must be a non-empty string")
    unknown = sorted(set(kwargs) - _SPAWN_ALLOWED_OPTIONS)
    if unknown:
        raise TypeError(f"rlm.spawn unknown options: {', '.join(unknown)}")


def _validate_spawn_limits(kwargs: dict[str, Any]) -> None:
    timeout_ms = kwargs.get("timeout_ms")
    if timeout_ms is not None:
        invalid_timeout = (
            isinstance(timeout_ms, bool)
            or not isinstance(timeout_ms, (int, float))
            or timeout_ms < 1
        )
        if invalid_timeout:
            raise ValueError("rlm.spawn timeout_ms must be a positive number")
    depth = kwargs.get("depth")
    if depth is not None:
        if isinstance(depth, bool) or not isinstance(depth, int) or depth < 0:
            raise ValueError("rlm.spawn depth must be a non-negative integer")


def _validate_spawn_mode(kwargs: dict[str, Any]) -> None:
    worktree = kwargs.get("worktree")
    if worktree is not None and worktree not in (True, False, "auto"):
        raise ValueError(
            'rlm.spawn worktree must be True, False or "auto" '
            "(every worktree child gets its own)"
        )
    if kwargs.get("fork") is not None and not isinstance(kwargs["fork"], bool):
        raise ValueError("rlm.spawn fork must be True or False")
    context = kwargs.get("context")
    if context is not None and context not in ("fresh", "fork", "retrieve"):
        raise ValueError('rlm.spawn context must be "fresh", "fork" or "retrieve"')
    setup = kwargs.get("worktree_setup")
    if setup is not None and not isinstance(setup, dict):
        raise ValueError(
            'rlm.spawn worktree_setup must be a dict: '
            '{"link": [...], "copy": [...], "command": "..."}'
        )


def _validate_spawn_cwd(kwargs: dict[str, Any]) -> None:
    cwd = kwargs.get("cwd")
    if cwd is None:
        return
    if not isinstance(cwd, (str, os.PathLike)) or not str(cwd).strip():
        raise ValueError("rlm.spawn cwd must be a directory path")
    kwargs["cwd"] = str(cwd)


def _normalize_merge_items(children: Any) -> list[Any]:
    if children is None:
        return []
    if isinstance(children, (str, SpawnHandle, dict)):
        return [children]
    return list(children)


def _dict_merge_selector(item: dict[str, Any]) -> str | None:
    if isinstance(item.get("id"), str):
        return item["id"]
    worktree = item.get("worktree")
    if isinstance(worktree, dict) and worktree.get("task"):
        return worktree["task"]
    result = item.get("result")
    if isinstance(result, dict) and isinstance(result.get("worktree"), dict):
        return result["worktree"]["task"]
    return None


def _merge_selector(item: Any) -> str:
    if isinstance(item, SpawnHandle):
        return item.rlm_child_id
    if isinstance(item, str) and item:
        return item
    if isinstance(item, dict):
        selected = _dict_merge_selector(item)
        if selected is not None:
            return selected
    raise TypeError(
        f"rlm.merge cannot tell which child {item!r} is: "
        "pass SpawnHandles, ids or results"
    )


class RLMNamespace:
    """Subagents and bounded inference.

    `h = await rlm.spawn(task, name=...)` starts a subagent with its own REPL (a full agent: it re-sends the
    system prompt and its own transcript on every turn, so use it for independent multi-step work, not for
    reading files); `await rlm.collect([h])` waits for results; a subagent ends with `rlm.finish(...)`;
    `worktree=True` gives a child its own Git worktree and `await rlm.merge(hs)` brings the work back
    (`help(rlm.merge)`, `rlm.worktrees`); `rlm.list_subagents()`, `rlm.progress()`, `rlm.delete_subagent(id)`. `rlm.load`, `rlm.open`, `rlm.infer`, `rlm.map` and `rlm.frames` are the
    bounded-inference API (`help(rlm.map)`). `rlm.jobs()` / `rlm.job(id)` recover shell jobs. `rlm.results()` never waits; `rlm.lanes()`, `rlm.lane`, `rlm.cells()`, `rlm.cell`, `rlm.recovery()`: REPL lanes (`help(rlm.lane)`).

    Search before delegating, over many files or a large input: search the concept and its synonyms in code,
    print one compact line per candidate, read the deciding passages of the unclear ones yourself, and use
    `rlm.map` only for what a line or two cannot settle. For example:

        import re
        from pathlib import Path
        docs = {p.stem: p.read_text() for p in Path("reports").glob("*.md")}
        topic = re.compile(r"certific|\bTLS\b|\bSSL\b|x\.?509", re.I)
        event = re.compile(r"expir|lapsed|notAfter|validity", re.I)
        hits = {k: [l.strip() for l in t.splitlines() if topic.search(l) and event.search(l)] for k, t in docs.items()}
        for k, lines in hits.items():
            if lines: print(k, " | ".join(l[:150] for l in lines[:3]))

    Then print the root-cause lines of the unclear candidates, decide, and check synonyms you may have missed.
    """

    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge
        self.worktrees = Worktrees(bridge)

    async def gather(
        self,
        coros: Any,
        *,
        limit: int = 8,
        return_exceptions: bool = True,
    ) -> list[Any]:
        """Run awaitables concurrently with bounded concurrency (default limit=8), preserving order.

        By default ``return_exceptions=True`` so one failure does not cancel sibling calls (failed items are
        returned as their Exception instance); pass ``return_exceptions=False`` to raise on the first error.
        """
        if isinstance(limit, bool) or not isinstance(limit, int) or limit < 1:
            raise ValueError("limit must be a positive integer")
        items = list(coros)
        if not items:
            return []
        sem = asyncio.Semaphore(limit)

        async def _run(awaitable: Any) -> Any:
            async with sem:
                return await awaitable

        return list(
            await asyncio.gather(
                *(_run(item) for item in items),
                return_exceptions=return_exceptions,
            )
        )

    async def spawn(self, prompt: str, **kwargs: Any) -> SpawnHandle:
        """Start a subagent and return its SpawnHandle at once (`h.rlm_child_id`); options: name (required),
        model, thinking, timeout_ms, depth, worktree, worktree_setup, cwd, context, fork.

        The child has its own REPL and your tools and shares your filesystem, but not your conversation: give
        a self-contained brief (goal, paths, constraints, what to return). Its final reply is its result.
        context="fresh" (the default) is that. context="retrieve" also tells it where this session is stored, so
        it can look up what your brief left out with `sessions.search` / `sessions.read` instead of guessing.
        context="fork" (or fork=True) starts it on your conversation so far instead (not your kernel's
        variables), so a short brief does; it re-reads all of that context every turn, so fork only when the
        child needs most of what you learned.
        Start several at once, then do only separate work of your own: never check on children through their
        files, logs or progress, since their results come to you. With nothing of your own left, wait for free
        with `await rlm.collect(handles)`, or (completion events on) end your turn: each end arrives as a
        `child_done` event.
        `await rlm.collect([h])` returns `[{"id": ..., "result": {"status": "succeeded", "value": <answer>,
        "verdict": {...} | None, "check": {"outcome": ...}}}]` (check each status): trust a verdict only when
        `check["outcome"] == "verified"`; `unobserved` lists files it claimed but did not change, `unreported`
        files that changed while it ran without being declared (maybe by concurrent work). A subagent does its brief itself unless you pass `depth=N` (1 lets it spawn its
        own subagents, 2 lets those spawn too; at most 3 levels in all), and every response they make counts
        toward the root's turn, token and cost limits. Spawn for independent multi-step work, never to read or
        classify documents (narrow with code and `rlm.map` instead).

        worktree=True: the child works in its own private Git worktree (`h.worktree["branch"]`), branched from
        your tree as it is now (uncommitted changes included), so children editing the same files cannot clobber
        each other. Use it whenever several children edit files. Put tightly coupled tasks (the same functions,
        or one building on another) in ONE child's brief, independent ones in their own children, then
        `await rlm.merge(hs)` to bring their work into your tree. Its result gains `worktree` (branch, commit,
        changed_files, diffstat). worktree="auto" falls back to your shared tree outside Git. Gitignored
        node_modules/.venv/venv are linked in and small .env files copied; worktree_setup={"link": [...],
        "copy": [...], "command": "npm ci"} changes that (linked dependencies are shared: children must not
        install different ones; servers they start share your ports). Its own children branch from its worktree.
        cwd="path" (absolute or relative to your working directory; it must exist) runs the child there; with
        worktree=True the worktree branches from the Git repository at that path and `rlm.merge` merges back into
        it, so a session whose directory only holds a repository can still use worktrees. keep=True refuses it."""
        _validate_spawn_core(prompt, kwargs)
        _validate_spawn_limits(kwargs)
        _validate_spawn_mode(kwargs)
        _validate_spawn_cwd(kwargs)
        result = await self._bridge.request("rlm.spawn", {
            "prompt": prompt,
            "kwargs": kwargs,
        })
        return SpawnHandle(**result)

    async def finish(self, status: str, summary: str, *, evidence: Any = (), outputs: dict[str, Any] | None = None,
                     changed_files: Any = (), left: Any = ()) -> dict[str, Any]:
        """As a subagent, record your verdict, then end your turn with a short reply.

        status: "passed" (the brief is done), "failed" (it could not be done) or "blocked" (something outside
        you stopped it). summary: what was done or what blocked it. evidence: concrete strings, each a command
        with its outcome ("pytest -q: exit 0, 12 passed") or a file with lines ("src/a.py:40-52 handles X");
        "passed" is rejected without it. outputs: named results for the parent. changed_files: every file
        you (or your subagents) created, edited or deleted, relative to the working directory. left: one string
        for each thing you left undone or found broken outside your brief; your parent must settle each.

        The host checks changed_files against the files that changed on disk while you ran and shows the
        parent any mismatch. A rejected call raises with the reasons: fix them and call again (a few attempts)."""
        payload: dict[str, Any] = {
            "status": status,
            "summary": summary,
            "evidence": list(evidence) if isinstance(evidence, (list, tuple)) else evidence,
            "outputs": {} if outputs is None else outputs,
            "changed_files": [str(p) for p in changed_files] if isinstance(changed_files, (list, tuple, set)) else changed_files,
        }
        if left:
            payload["left"] = list(left) if isinstance(left, (list, tuple)) else left
        return await self._bridge.request("rlm.finish", payload)

    async def merge(self, children: Any = None, *, strategy: str = "sequential", on_conflict: str = "stop",
                    include_failed: bool = False) -> dict[str, Any]:
        """Bring worktree children's work into your working tree, in the order given (waits for them first).

        children: SpawnHandles, ids, `rlm.collect` items or results with a `worktree` (workflow outcomes); all of
        your worktree children when omitted. Each child's branch is three-way merged into your tree as uncommitted
        changes (new files untracked); nothing is committed on your branch, so review and commit yourself.
        The first conflict stops the rest and writes nothing of that child; its report names the files and hunks
        (`on_conflict="skip"` goes on with the next child; `"markers"` writes conflict markers to resolve).
        Children that did not pass are skipped unless include_failed=True. Returns `{"ok": bool, "merged": [ids],
        "results": [{"id", "name", "branch", "status": "merged"|"empty"|"conflict"|"skipped"|"not_attempted"|
        "no_worktree"|"failed", "files", "conflicts": [{"path", "hunks": [{"line", "ours", "theirs"}]}]}]}`.
        A merged or empty child's worktree and branch are removed; a conflicting one is kept for inspection."""
        if strategy != "sequential":
            raise ValueError('rlm.merge strategy must be "sequential"')
        if on_conflict not in ("stop", "skip", "markers"):
            raise ValueError('rlm.merge on_conflict must be "stop", "skip" or "markers"')
        items = _normalize_merge_items(children)
        selectors = [_merge_selector(item) for item in items]
        return await self._bridge.request("rlm.merge", {
            "selectors": selectors,
            "on_conflict": on_conflict,
            "include_failed": bool(include_failed),
        })

    async def list_subagents(self) -> list[dict[str, Any]]:
        result = await self._bridge.request("rlm.list_subagents")
        return result.get("subagents", []) if isinstance(result, dict) else []

    async def collect(self, selectors: list[str] | None = None, timeout_ms: int = 0) -> list[dict[str, Any]]:
        """Results of subagents (SpawnHandles or ids; all of yours when empty):
        `[{"id": ..., "result": {"status": ..., "value" | "error": ..., "verdict": ..., "check": ...}}]`
        (`help(rlm.spawn)`). It waits for them, except where the host waits for children itself: there, with every
        selected child ended it returns their results, and otherwise raises RuntimeError naming the children still
        running (each end arrives as a `child_done` event: do other work or end your turn, then collect again).
        `rlm.results()` never waits: it returns the results of children that ended and the ids of those still
        running."""
        result = await self._bridge.request("rlm.collect", {
            "selectors": child_selectors(selectors),
            "timeout_ms": timeout_ms,
        })
        return result.get("results", []) if isinstance(result, dict) else []

    async def progress(self, selectors: list[str] | None = None) -> list[dict[str, Any]]:
        """How your running subagents are doing (SpawnHandles or ids; all of yours when empty), for when the user
        asks: `[{"id", "name", "level", "description", "responses", "cells", "elapsed_s", "cost_usd",
        "forecast": {"time_left_s_max", "cost_usd_per_min", "cost_usd_at_timeout"}}]`. Levels are described
        stages read from each child's transcript, not percentages: starting, exploring (nothing changed yet),
        changing (edits not checked yet), checking (a test, build or linter ran after its last edit), finishing
        (it recorded its verdict) or stalled (failing cells in a row, or idle for minutes). The forecasts bound
        its remaining time by its timeout and project its cost at its spend rate so far. Finished children are
        left out (collect them). Never poll it while you wait: their results come to you."""
        result = await self._bridge.request("rlm.progress", {"selectors": child_selectors(selectors)})
        return result.get("subagents", []) if isinstance(result, dict) else []

    async def delete_subagent(self, selector: str) -> dict[str, Any]:
        result = await self._bridge.request("rlm.delete_subagent", {"selector": selector})
        return result if isinstance(result, dict) else {"result": result}

    async def find_models(self, query: str = "", limit: int = 8) -> list[dict[str, Any]]:
        result = await self._bridge.request("rlm.find_models", {"query": query, "limit": limit})
        return _matching_models(result, query, limit)

    async def host_request(self, request_type: str, payload: dict[str, Any] | None = None) -> Any:
        return await self._bridge.request(request_type, payload)

    async def jobs(self) -> list["ShellJob"]:
        """Shell jobs started with bash(..., yield_after=...), newest first (without their text)."""
        result = await self._bridge.request("shell.list", {})
        return [ShellJob(self._bridge, item) for item in (result if isinstance(result, list) else [])]

    async def job(self, job_id: str) -> "ShellJob":
        """Recover a shell job's handle by id (after the variable was lost or the kernel restarted)."""
        if not isinstance(job_id, str) or not job_id:
            raise ValueError("job id must be a non-empty string")
        return ShellJob(self._bridge, await self._bridge.request("shell.get", {"id": job_id}))

class BackgroundNamespace:
    """Long-running background agent jobs that outlive the turn.

    `await background.start(prompt)` starts one; `background.list()`, `background.inspect(id)`,
    `background.result(id)` and `background.stop(id)` manage them. Completion arrives as a `task_done` event.
    """

    def __init__(self, bridge: HostBridge) -> None:
        self._bridge = bridge

    async def start(self, prompt: str, *, model: str | None = None, key: str | None = None, timeout_ms: int | None = None) -> dict[str, Any]:
        if not isinstance(prompt, str) or not prompt.strip():
            raise ValueError("background.start prompt must be non-empty")
        payload: dict[str, Any] = {"prompt": prompt}
        if model is not None:
            payload["model"] = model
        if key is not None:
            payload["key"] = key
        if timeout_ms is not None:
            payload["timeout_ms"] = timeout_ms
        result = await self._bridge.request("background.start", payload)
        return result if isinstance(result, dict) else {"result": result}

    async def list(self) -> list[dict[str, Any]]:
        result = await self._bridge.request("background.list", {})
        return result if isinstance(result, list) else []

    async def inspect(self, job_id: str) -> dict[str, Any]:
        return await self._bridge.request("background.inspect", {"id": job_id})

    async def stop(self, job_id: str) -> dict[str, Any]:
        return await self._bridge.request("background.stop", {"id": job_id})

    async def result(self, job_id: str) -> Any:
        return await self._bridge.request("background.result", {"id": job_id})


# A command's full output is read back from the host's spill file up to this size.
_BASH_MAX_OUTPUT_BYTES = 64 * 1024 * 1024


def _format_running_job_note(job: "ShellJob", waited: float | None) -> str:
    events = waits.async_events_enabled()
    waited_text = f" after {waited:g} s" if waited is not None else ""
    if events:
        return (
            f"\n[still running as job {job.id}{waited_text}; its completion will arrive as a "
            "runtime event, or `await <result>.job.result()` to wait]"
        )
    return (
        f"\n[still running as job {job.id}{waited_text}; "
        "`await <result>.job.result()` waits for it]"
    )


def _format_termination_note(
    timed_out: bool,
    cancelled: bool,
    exit_code: int | None,
    timeout: float | None,
    notes: dict[str, Any],
) -> str:
    if timed_out:
        return f"\n[timed out after {timeout:g}s]"
    if cancelled:
        return "\n[cancelled]"
    if exit_code not in (0, None):
        return _bash_exit_lines(exit_code, notes)
    return ""


def _format_truncation_note(truncated: bool, full_output_path: str | None) -> str:
    if not truncated:
        return ""
    if full_output_path:
        return f"\n[output truncated; full output in {full_output_path}]"
    return "\n[output truncated]"


class BashOutput(str):
    """The combined stdout/stderr of a shell command, as a string (nano-rlm's bash skill).

    A nonzero exit, a timeout or a cancellation is appended as a bracketed status line, so
    printing the string always shows whether the command failed. The details are attributes:
    .output (raw output), .exit_code, .ok, .timed_out, .cancelled, .truncated (output cut
    short), .full_output_path. Earlier cells used a dict; ``out["exit_code"]`` still works.

    A command still running when ``bash`` stopped waiting (the default ``yield_after``) keeps
    running as a host job: then .running is True, .job is its ShellJob, .exit_code is None,
    .ok is False, the text is the output so far plus a note, and its end arrives as an event.
    """

    output: str
    exit_code: int | None
    timed_out: bool
    cancelled: bool
    truncated: bool
    full_output_path: str | None
    running: bool
    job: "ShellJob | None"

    def __new__(cls, output: str, exit_code: int | None, *, timed_out: bool = False, cancelled: bool = False,
                truncated: bool = False, full_output_path: str | None = None, timeout: float | None = None,
                job: "ShellJob | None" = None, waited: float | None = None, **notes: Any) -> "BashOutput":
        text = output.strip()
        running = job is not None and job.running
        if running and job is not None:
            text += _format_running_job_note(job, waited)
        else:
            text += _format_termination_note(timed_out, cancelled, exit_code, timeout, notes)
        text += _format_truncation_note(truncated, full_output_path)
        self = super().__new__(cls, text.strip() or "(no output)")
        self.output = output
        self.exit_code = exit_code
        self.timed_out = timed_out
        self.cancelled = cancelled
        self.truncated = truncated
        self.full_output_path = full_output_path
        self.running = running
        self.job, self.exit_note, self.sandbox_note = job, notes.get("exit_note"), notes.get("sandbox_note")
        return self

    @property
    def ok(self) -> bool:
        return self.exit_code == 0 and not self.timed_out and not self.cancelled and not self.running

    @property
    def text(self) -> str:
        """The string itself (as ShellJob.text), for code written against a result object."""
        return str(self)

    def __getitem__(self, key: Any) -> Any:
        if isinstance(key, str):
            if key in ("output", "exit_code", "timed_out", "cancelled", "truncated", "full_output_path", "ok", "running", "job"):
                return getattr(self, key)
            raise KeyError(key)
        return str.__getitem__(self, key)

    def get(self, key: str, default: Any = None) -> Any:
        try:
            return self[key]
        except KeyError:
            return default


def _str_field(data: dict[str, Any], key: str) -> str:
    return str(data.get(key) or "")


def _opt_str_field(data: dict[str, Any], key: str) -> str | None:
    value = data.get(key)
    return value if isinstance(value, str) else None


class ShellJob:
    """A shell command owned by the Ultron host (started with ``bash(cmd, yield_after=...)``).

    It keeps running after the cell ends, across kernel restarts and eviction; an Esc abort of the
    turn that started it, ``timeout=``, or ``await job.cancel()`` stops it. When it ends while you
    are not waiting on it, a ``<runtime_event kind="job_done">`` message arrives on its own: do not poll.

    Attributes: .id, .running, .status, .exit_code, .ok (clean exit 0), .text (combined output,
    at most 16 KiB: head and tail around a marker), .truncated, .timed_out, .cancelled,
    .output_path, .output_bytes, .elapsed_seconds, .error.
    """

    def __init__(self, bridge: "HostBridge", data: Any) -> None:
        self._bridge = bridge
        self._update(data)

    def _update(self, data: Any) -> None:
        if not isinstance(data, dict) or not isinstance(data.get("id"), str):
            raise RuntimeError("bash: the host returned no job")
        self.id: str = data["id"]
        self.command: str = _str_field(data, "command")
        self.status: str = _str_field(data, "status")
        self.running: bool = bool(data.get("running"))
        exit_code = data.get("exit_code")
        self.exit_code: int | None = exit_code if isinstance(exit_code, int) else None
        self.ok: bool = bool(data.get("ok"))
        self.text: str | None = _opt_str_field(data, "text")
        self.truncated: bool = bool(data.get("truncated"))
        self.timed_out: bool = bool(data.get("timed_out"))
        self.cancelled: bool = bool(data.get("cancelled"))
        self.output_path: str = _str_field(data, "output_path")
        self.output_bytes: int = int(data.get("output_bytes") or 0)
        self.elapsed_seconds: float = float(data.get("elapsed_seconds") or 0)
        self.error, self.exit_note, self.sandbox_note = (
            _opt_str_field(data, "error"),
            *_bash_notes(data).values(),
        )

    @property
    def job(self) -> "ShellJob":
        """This handle, so ``out.job.result()`` works whether ``out`` is a BashOutput or a ShellJob."""
        return self

    async def result(self, wait: float | None = None) -> "ShellJob":
        """Wait for the job to end (at most ``wait`` seconds) and return this handle, refreshed."""
        if wait is not None and (isinstance(wait, bool) or not isinstance(wait, (int, float)) or wait < 0):
            raise ValueError("wait must be a non-negative number of seconds")
        payload: dict[str, Any] = {"id": self.id}
        if wait is not None:
            payload["wait"] = wait
        self._update(await self._bridge.request("shell.result", payload))
        return self

    async def cancel(self) -> "ShellJob":
        """Stop the job (its whole process tree) and return this handle, refreshed."""
        self._update(await self._bridge.request("shell.cancel", {"id": self.id}))
        return self

    async def read(self, cursor: int = 0, max_bytes: int = 65536) -> dict[str, Any]:
        """Read retained output from byte ``cursor``: {text, next_cursor, done, truncated}."""
        return await self._bridge.request("shell.read", {"id": self.id, "cursor": cursor, "max_bytes": max_bytes})

    def _status_line(self) -> str:
        if self.running:
            return f"[job {self.id} running {self.elapsed_seconds:g}s: completion arrives as a <runtime_event>; await job.result() to wait]"
        if self.status == "completed":
            return f"[job {self.id} {_exit_words(self.exit_code, self.exit_note)} after {self.elapsed_seconds:g}s]"
        detail = f": {self.error}" if self.error else ""
        return f"[job {self.id} {self.status}{detail}]"

    def __str__(self) -> str:
        body = "\n".join(part for part in ((self.text or "").strip(), self.sandbox_note) if part)
        return f"{body}\n{self._status_line()}" if body else self._status_line()

    def __repr__(self) -> str:
        if self.text is None:
            return f"ShellJob(id={self.id!r}, status={self.status!r}, exit_code={self.exit_code!r}, command={self.command[:60]!r})"
        return str(self)


# Outside a work tree `git diff` prints its whole option list (about 130 lines, 7 KB) after the one line that
# matters; every later turn re-reads it. Keep the warning and drop the list.
_GIT_DIFF_USAGE = re.compile(
    r"^usage: git diff --no-index [^\n]*\n(?:(?:[ \t][^\n]*|Diff [^\n]*|Other diff options[^\n]*|)(?:\n|$))*",
    re.MULTILINE,
)


def _quiet_git_usage(output: str) -> str:
    if "usage: git diff --no-index" not in output:
        return output
    return _GIT_DIFF_USAGE.sub("(not a git work tree: no diff; compare against the text you read instead)\n", output)


def _validate_bash_args(command: str, timeout: float | None) -> None:
    if not isinstance(command, str) or not command.strip():
        raise ValueError("bash command must be a non-empty string")
    if timeout is not None:
        invalid_timeout = (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or timeout <= 0
        )
        if invalid_timeout:
            raise ValueError("bash timeout must be a positive number of seconds")


def _resolve_yield_after(yield_after: Any) -> tuple[bool, float | None]:
    if yield_after is waits.DEFAULT:
        return False, waits.setting("ULTRON_BASH_YIELD_AFTER")
    if yield_after is None:
        return False, None
    invalid_wait = (
        isinstance(yield_after, bool)
        or not isinstance(yield_after, (int, float))
        or yield_after < 0
    )
    if invalid_wait:
        raise ValueError("bash yield_after must be a non-negative number of seconds")
    return True, min(float(yield_after), waits.MAX_WAIT_SECONDS)


def _bash_job_payload(
    command: str, timeout: float | None, wait_seconds: float | None
) -> dict[str, Any]:
    fields: dict[str, Any] = {"command": command, "yield_after": wait_seconds}
    if timeout is not None:
        fields["timeout"] = timeout
    return fields


def _bash_run_payload(
    command: str, timeout: float | None, auto: float | None
) -> dict[str, Any]:
    fields: dict[str, Any] = {"command": command}
    if timeout is not None:
        fields["timeout"] = timeout
    if auto is not None:
        fields["yield_after"] = auto
    return fields


def _read_spill_output(result: dict[str, Any], job: ShellJob | None) -> tuple[str, bool]:
    output = str(result.get("output") or "")
    truncated = bool(result.get("truncated"))
    path = result.get("full_output_path")
    can_read_spill = (
        truncated
        and isinstance(path, str)
        and bool(path)
        and job is None
        and not result.get("partial_file")
    )
    if not can_read_spill:
        return output, truncated
    # The host keeps only the tail; the whole output is in its spill file.
    with contextlib.suppress(OSError):
        with open(path, "rb") as handle:
            data = handle.read(_BASH_MAX_OUTPUT_BYTES + 1)
        if len(data) <= _BASH_MAX_OUTPUT_BYTES:
            return data.decode("utf-8", errors="replace"), False
    return output, truncated


def _build_bash_output(
    result: dict[str, Any], timeout: float | None, auto: float | None
) -> BashOutput:
    job_data = result.get("job")
    running = bool(result.get("running")) and isinstance(job_data, dict)
    job = ShellJob(_STATE.bridge, job_data) if running else None
    output, truncated = _read_spill_output(result, job)
    path = result.get("full_output_path")
    exit_code = result.get("exit_code")
    return BashOutput(
        _quiet_git_usage(output),
        exit_code if isinstance(exit_code, int) else None,
        timed_out=bool(result.get("timed_out")),
        cancelled=bool(result.get("cancelled")),
        truncated=truncated,
        full_output_path=path if isinstance(path, str) else None,
        timeout=timeout,
        job=job,
        waited=auto,
        **_bash_notes(result),
    )


async def bash(command: str, timeout: float | None = None, yield_after: Any = waits.DEFAULT) -> Any:
    """Run a shell command in the working directory.

    Args:
        command: The command, run by the user's shell (bash). A string literal written in the call
            reaches bash as typed: its backslashes are not Python escapes (as if written r'''...'''),
            so a heredoc's program keeps its own '\\n'.
        timeout: Seconds before the command is killed (default: no limit).
        yield_after: Left out: wait up to ULTRON_BASH_YIELD_AFTER seconds (default 30) and return
            the output as a string; a command still running then keeps running as a host job and
            the string says so (.running True, .job its ShellJob; its completion arrives as a
            <runtime_event>). None: wait for the command however long it takes. A number of
            seconds: start the command as a host job and return a ShellJob after at most that
            long, finished (.running False) or still running; 0 returns at once.

    Returns:
        Without a yield_after number, a BashOutput: stdout and stderr combined, with "[exit code N]"
        appended when the command failed; .exit_code and .ok carry the status. With a number, a
        ShellJob (see help(ShellJob)).
    """
    _validate_bash_args(command, timeout)
    job_mode, wait_seconds = _resolve_yield_after(yield_after)
    if job_mode:
        job_payload = _bash_job_payload(command, timeout, wait_seconds)
        return ShellJob(_STATE.bridge, await _STATE.bridge.request("shell.run", job_payload))
    payload = _bash_run_payload(command, timeout, wait_seconds)
    result = await _STATE.bridge.request("bash", payload)
    if not isinstance(result, dict):
        raise RuntimeError("bash: the host returned no result")
    return _build_bash_output(result, timeout, wait_seconds)


# `read` returns a file's text up to this size and a ContextHandle above it (ULTRON_READ_HANDLE_BYTES).
_DEFAULT_READ_HANDLE_BYTES = 256 * 1024


def _read_handle_bytes() -> int:
    try:
        value = int(os.environ.get("ULTRON_READ_HANDLE_BYTES", "").strip())
    except ValueError:
        return _DEFAULT_READ_HANDLE_BYTES
    return value if value > 0 else _DEFAULT_READ_HANDLE_BYTES


async def read(path: str | os.PathLike[str]) -> Any:
    """Read a text file (relative to the working directory, or absolute).

    Returns the text (a str) for a file up to ULTRON_READ_HANDLE_BYTES (default 256 KiB). A larger
    file is loaded with ``rlm.load`` and returned as a ContextHandle, with a one-line note naming its
    size and digest: program over it with ``h.search(regex)``, ``h.lines(a, b)``, ``h.chunks(n)``,
    ``h.count(regex)`` or ``rlm.map(task, h.chunks(n))`` instead of reading it whole.
    """
    if not isinstance(path, (str, os.PathLike)) or not str(path):
        raise ValueError("read path must be a non-empty string")
    filepath = worktree_path(Path(path))
    if not filepath.is_absolute():
        filepath = Path.cwd() / filepath
    size = filepath.stat().st_size
    with filepath.open("rb") as handle:
        kind = _image_kind(handle.read(16))
    if kind is not None:
        return (
            f"[read] {path} is a {kind.upper()} image ({size:,} bytes); read() returns text only. "
            f"Use `await view_image({str(path)!r})` to look at it."
        )
    if size <= _read_handle_bytes():
        return filepath.read_bytes().decode("utf-8", errors="replace")
    handle = await _STATE.namespace["rlm"].load(path=filepath)
    print(
        f"[read] {path} is {size:,} bytes (over {_read_handle_bytes():,}), so it was loaded as a handle, not text: "
        f"{handle!r}. Use h.search(regex), h.lines(a, b), h.chunks(n) or rlm.map(task, h.chunks(n)) on it."
    )
    return handle


def _image_kind(head: bytes) -> str | None:
    """The image format named by a file's magic bytes (png, jpeg, gif, webp), or None."""
    if head.startswith(b"\x89PNG\r\n\x1a\n"):
        return "png"
    if head.startswith(b"\xff\xd8\xff"):
        return "jpeg"
    if head.startswith((b"GIF87a", b"GIF89a")):
        return "gif"
    if len(head) >= 12 and head[:4] == b"RIFF" and head[8:12] == b"WEBP":
        return "webp"
    return None


def _figure_png(source: Any) -> bytes | None:
    """PNG bytes of a matplotlib figure (or pyplot, or an Axes), else None."""
    figure = source
    if not hasattr(figure, "savefig") and hasattr(figure, "get_figure"):
        figure = figure.get_figure()
    if not callable(getattr(figure, "savefig", None)):
        return None
    buffer = io.BytesIO()
    figure.savefig(buffer, format="png", bbox_inches="tight")
    return buffer.getvalue()


def _pil_png(source: Any) -> bytes | None:
    """PNG bytes of a PIL image when PIL is available, else None."""
    try:
        from PIL import Image as PILImage  # type: ignore[import-not-found]
    except Exception:
        return None
    if not isinstance(source, PILImage.Image):
        return None
    image = source
    if image.mode not in ("1", "L", "LA", "P", "RGB", "RGBA"):
        image = image.convert("RGBA")
    buffer = io.BytesIO()
    image.save(buffer, format="PNG")
    return buffer.getvalue()


def _resolve_image_input(path_or_bytes: Any) -> tuple[Path | None, bytes | None]:
    if isinstance(path_or_bytes, (str, os.PathLike)):
        if not str(path_or_bytes):
            raise ValueError("view_image path must be a non-empty string")
        filepath = worktree_path(Path(path_or_bytes))
        if not filepath.is_absolute():
            filepath = Path.cwd() / filepath
        if not filepath.is_file():
            raise FileNotFoundError(f"{path_or_bytes} not found")
        with filepath.open("rb") as handle:
            if _image_kind(handle.read(16)) is None:
                raise ValueError(f"{path_or_bytes} is not a png, jpeg, gif or webp image")
        return filepath, None
    if isinstance(path_or_bytes, (bytes, bytearray, memoryview)):
        return None, bytes(path_or_bytes)
    data = _pil_png(path_or_bytes)
    if data is None:
        data = _figure_png(path_or_bytes)
    if data is None:
        raise TypeError(
            "view_image takes a path, image bytes, a PIL image or a matplotlib figure, "
            f"not {type(path_or_bytes).__name__}"
        )
    return None, data


def _write_temp_image(data: bytes) -> tuple[Path, str]:
    kind = _image_kind(data[:16])
    if kind is None:
        raise ValueError("view_image bytes are not a png, jpeg, gif or webp image")
    import tempfile

    descriptor, temporary = tempfile.mkstemp(prefix="ultron-view-image-", suffix=f".{kind}")
    with os.fdopen(descriptor, "wb") as handle:
        handle.write(data)
    return Path(temporary), temporary


def _cleanup_temp_image(temporary: str | None) -> None:
    if temporary is not None:
        with contextlib.suppress(OSError):
            os.unlink(temporary)


def _format_view_image_reply(reply: Any) -> str:
    if not isinstance(reply, dict):
        return str(reply)
    description = str(reply.get("description", "image attached"))
    note = reply.get("note")
    return f"{description}\n{note}" if note else description


async def view_image(path_or_bytes: Any, *, detail: str | None = None) -> str:
    """Show an image to the model: it is attached to this cell's tool result as an image.

    Accepts a path (png, jpeg, gif or webp, detected from the file's bytes, not its extension), raw image bytes,
    a PIL image, or a matplotlib figure (rendered to PNG). Oversized images are downscaled to the model's limits;
    ``detail="low"`` caps the long side at 512 px. At most 8 images per cell. Returns a short description
    (dimensions, format, bytes). The image itself never appears in printed output.
    """
    if detail is not None and detail not in ("low", "high", "auto"):
        raise ValueError("view_image detail must be None, 'low', 'high' or 'auto'")
    filepath, data = _resolve_image_input(path_or_bytes)
    temporary: str | None = None
    if data is not None:
        filepath, temporary = _write_temp_image(data)
    payload: dict[str, Any] = {"path": str(filepath)}
    if detail is not None:
        payload["detail"] = detail
    try:
        reply = await _STATE.bridge.request("rlm.view_image", payload)
    finally:
        _cleanup_temp_image(temporary)
    return _format_view_image_reply(reply)


_FILE_HOOKS_STICKY = os.environ.get("ULTRON_FILE_HOOKS") == "1"


def _file_hooks_enabled() -> bool:
    """Once enabled (at startup or in a session), file hooks stay on and cannot be disabled via os.environ."""
    global _FILE_HOOKS_STICKY
    if not _FILE_HOOKS_STICKY and os.environ.get("ULTRON_FILE_HOOKS") == "1":
        _FILE_HOOKS_STICKY = True
    return _FILE_HOOKS_STICKY


def _extract_write_hook_result(reply: Any) -> dict[str, Any]:
    results = reply.get("results") if isinstance(reply, dict) else None
    if isinstance(results, list) and results and isinstance(results[0], dict):
        return results[0]
    return {}


async def _check_write(filepath: Path, content: str, display: str) -> None:
    """Show a proposed write to the host's before-write hooks (Loki, extensions) before it happens.

    A hook that blocks or a host check failure raises ValueError and the caller writes nothing (fail closed).
    Individual guard timeouts/errors are handled by the host itself as advisory notes.
    """
    if not _file_hooks_enabled():
        return
    try:
        reply = await _STATE.bridge.request(
            "files.before_write", {"writes": [{"path": str(filepath), "content": content}]}
        )
    except (RuntimeError, ValueError) as error:
        raise ValueError(f"{display} was not written: write hook check failed: {error}") from None
    result = _extract_write_hook_result(reply)
    for note in result.get("notes") or []:
        print(note)
    if result.get("blocked"):
        raise ValueError(f"{display} was not written: {result.get('reason') or 'a write hook blocked it'}")


# write() and edit() wait on the host's check between reading a file and writing it, so two of them on one file in
# one gather would each write back the copy it read. They take turns per file; a lock lives while someone holds it.
_PATH_LOCKS: weakref.WeakValueDictionary[str, asyncio.Lock] = weakref.WeakValueDictionary()


def _path_lock(filepath: Path) -> asyncio.Lock:
    key = os.path.realpath(filepath)
    lock = _PATH_LOCKS.get(key)
    if lock is None:
        lock = _PATH_LOCKS[key] = asyncio.Lock()
    return lock


async def write(path: str | os.PathLike[str], text: str) -> str:
    """Create or overwrite a text file (parent directories are created).

    Args:
        path: File path, relative to the working directory or absolute.
        text: The complete new content.

    Returns:
        A confirmation message. The write is checked first (Loki guardrails, extension hooks): a check that
        blocks raises ValueError with its finding and nothing is written. Several writes in one cell are
        checked in parallel with ``await asyncio.gather(write(a, x), edit(b, ...))``.
    """
    if not isinstance(path, (str, os.PathLike)) or not str(path):
        raise ValueError("write path must be a non-empty string")
    if not isinstance(text, str):
        raise TypeError(f"write text must be a str, not {type(text).__name__}")
    filepath = worktree_path(Path(path))
    if not filepath.is_absolute():
        filepath = Path.cwd() / filepath
    async with _path_lock(filepath):
        await _check_write(filepath, text, str(path))
        filepath.parent.mkdir(parents=True, exist_ok=True)
        filepath.write_text(text)
    return f"Wrote {path} ({len(text.encode('utf-8')):,} bytes)"


async def edit(path: str, old_str: str, new_str: str) -> str:
    """Replace a unique string in a file (nano-rlm's edit skill).

    Args:
        path: File path, relative to the working directory or absolute.
        old_str: Exact string to find; it must appear exactly once in the file.
        new_str: Replacement string.

    Returns:
        A confirmation message. Raises FileNotFoundError when the file is missing and
        ValueError when old_str is absent or appears more than once, when a write check
        (Loki guardrails, extension hooks) blocks the new content, or when something else changed
        the file while it was being checked; then nothing is written.
    """
    filepath = worktree_path(Path(path))
    if not filepath.is_absolute():
        filepath = Path.cwd() / filepath
    async with _path_lock(filepath):
        if not filepath.exists():
            raise FileNotFoundError(f"{path} not found")
        content = filepath.read_text()
        count = content.count(old_str)
        if count == 0 and isinstance(old_str, str):
            marker = REDACTION_MARKER.search(old_str)
            if marker is not None:
                # Cell output masks secrets, so text copied from it can hold a marker the file does not. Nothing is written.
                raise ValueError(
                    f"old_str contains {marker.group(0)}, a mask that cell output shows in place of a secret; {path} "
                    "holds the real value, unchanged. Make old_str the text before or after the secret (not the marker), "
                    "or edit in code: `text = await read(path)` has the real content."
                )
        if count != 1:
            raise ValueError(f"old_str must appear exactly once in {path} (found {count})")
        updated = content.replace(old_str, new_str, 1)
        await _check_write(filepath, updated, str(path))
        # Writers this kernel's lock cannot see: another lane, a subagent in the same tree, a shell command.
        try:
            unchanged = filepath.read_text() == content
        except OSError:
            unchanged = False
        if not unchanged:
            raise ValueError(
                f"{path} was not edited: it changed on disk while the edit was being checked. "
                "Read it again and redo the edit."
            )
        filepath.write_text(updated)
    return f"Edited {path}"


def _env_limit(name: str) -> int:
    try:
        return max(0, int(os.environ.get(name, "0")))
    except ValueError:
        return 0


# Resource limits come from the host, which resolves defaults and overrides. 0 disables one.
# Memory is RLIMIT_DATA per process, inherited by every subprocess a cell spawns. CPU is a per-cell budget:
# the soft RLIMIT_CPU is re-armed at each cell start, and the host kills the kernel if a cell overruns it.
_MAX_MEMORY_MB = _env_limit("ULTRON_RLM_MAX_MEMORY_MB")
_MAX_CPU_SECONDS = _env_limit("ULTRON_RLM_MAX_CPU_SECONDS")
# Exit codes the host maps to "exceeded its memory/CPU limit" when the runtime cannot report in-band.
_EXIT_MEMORY = 86
_EXIT_CPU = 87
_EXIT_INTERNAL = 70


class RlmCpuLimitExceeded(BaseException):
    """Raised in the cell when it uses more than ULTRON_RLM_MAX_CPU_SECONDS of CPU time."""


def _cpu_used() -> float:
    import resource

    usage = resource.getrusage(resource.RUSAGE_SELF)
    return usage.ru_utime + usage.ru_stime


def _arm_cpu_limit() -> None:
    """Give the process `_MAX_CPU_SECONDS` more CPU before SIGXCPU; children inherit the soft limit."""
    if _MAX_CPU_SECONDS <= 0:
        return
    try:
        import resource

        _soft, hard = resource.getrlimit(resource.RLIMIT_CPU)
        soft = math.ceil(_cpu_used()) + _MAX_CPU_SECONDS
        if hard != resource.RLIM_INFINITY:
            soft = min(soft, hard)
        resource.setrlimit(resource.RLIMIT_CPU, (soft, hard))
    except (ImportError, OSError, ValueError):
        pass


def _on_cpu_limit(_signum: int, _frame: Any) -> None:
    # Re-arm first so the error path gets a fresh budget instead of a signal every second.
    _arm_cpu_limit()
    if _STATE.cell_active:
        raise RlmCpuLimitExceeded(
            f"RLM cell exceeded its CPU limit of {_MAX_CPU_SECONDS} CPU-seconds (ULTRON_RLM_MAX_CPU_SECONDS)"
        )


def _disable_core_dumps(resource: Any) -> None:
    # No process in the kernel's tree dumps core when hitting resource limits.
    if hasattr(resource, "RLIMIT_CORE"):
        with contextlib.suppress(ValueError, OSError):
            resource.setrlimit(resource.RLIMIT_CORE, (0, 0))


def _apply_memory_limit(resource: Any) -> None:
    # macOS reserves ~400 GiB of virtual address space on Apple Silicon, so RLIMIT_DATA
    # cannot meaningfully cap memory there.
    if _MAX_MEMORY_MB <= 0 or not hasattr(resource, "RLIMIT_DATA") or sys.platform == "darwin":
        return
    limit = _MAX_MEMORY_MB * 1024 * 1024
    with contextlib.suppress(ValueError, OSError):
        _soft, hard = resource.getrlimit(resource.RLIMIT_DATA)
        if hard != resource.RLIM_INFINITY:
            limit = min(limit, hard)
        # The hard limit is lowered too, so cell code cannot raise it again for itself or its children.
        resource.setrlimit(resource.RLIMIT_DATA, (limit, limit))


def _apply_resource_limits() -> None:
    try:
        import resource
    except ImportError:
        return
    _disable_core_dumps(resource)
    _apply_memory_limit(resource)
    if _MAX_CPU_SECONDS > 0 and hasattr(signal, "SIGXCPU"):
        signal.signal(signal.SIGXCPU, _on_cpu_limit)
        _arm_cpu_limit()


def _become_subreaper() -> None:
    # A cell may start a process that double-forks (or setsid()s and forks) so its parent exits. Linux would
    # reparent such an orphan to init or a subreaper outside the kernel, where the host's tree memory watchdog
    # neither counts nor kills it. As a child subreaper the kernel adopts every orphaned descendant instead.
    if not sys.platform.startswith("linux"):
        return
    try:
        import ctypes

        libc = ctypes.CDLL(None, use_errno=True)
        libc.prctl(36, 1, 0, 0, 0)  # PR_SET_CHILD_SUBREAPER
    except (OSError, AttributeError):
        pass


def _memory_limit_note() -> str:
    return (
        f"RLM kernel exceeded its memory limit ({_MAX_MEMORY_MB} MiB per process, ULTRON_RLM_MAX_MEMORY_MB); "
        "the allocation failed and the kernel keeps running"
    )


class RuntimeState:
    def __init__(self) -> None:
        self.bridge = HostBridge()
        self.cell_active = False
        self.active_task: asyncio.Task[Any] | None = None
        self.active_request_id: str | None = None
        self.execution_lock = asyncio.Lock()
        self.console = ConsoleBuffer()
        # asyncio is pre-imported: the guide batches independent work with asyncio.gather.
        self.namespace: dict[str, Any] = {"__name__": "__main__", "asyncio": asyncio}
        # The stdlib names cells most often use without importing (a NameError costs a whole turn).
        self.namespace.update({"re": re, "json": json, "os": os, "Path": Path})
        self.namespace["rlm"] = RLMNamespace(self.bridge)
        self.namespace["agent_message"] = AgentMessages(self.bridge)
        self.namespace["background"] = BackgroundNamespace(self.bridge)
        self.namespace["bash"] = bash
        self.namespace["edit"] = edit
        self.namespace["write"] = write
        self.namespace["read"] = read
        self.namespace["view_image"] = view_image
        self.namespace["hints"] = Hints(self.bridge)
        self.namespace["goal"] = Goal(self.bridge)
        # Extension tools (the built-in `mcp` gateway among them) as async skills.
        self.namespace["tools"] = Tools(self.bridge)
        self.namespace["mcp"] = Mcp(self.namespace["tools"])
        self.namespace["ToolCall"] = ToolCall
        self.namespace["ToolResult"] = ToolResult
        self.namespace["ToolError"] = ToolError
        self.namespace["McpError"] = McpError
        self.namespace["SpawnHandle"] = SpawnHandle
        self.namespace["ShellJob"] = ShellJob
        self.namespace["agents"] = Agents(self.bridge)
        self.namespace["workflows"] = Workflows(self.bridge)
        self.namespace["budget"] = BudgetEstimator(self.bridge)
        self.namespace["memory"] = Memory(self.bridge)
        self.namespace["refinements"] = Refinements(self.bridge)
        self.namespace["progress"] = Progress(self.bridge)
        self.namespace["skills"] = Skills(self.bridge, self.namespace)
        self.namespace["schedules"] = Schedules(self.bridge)
        self.namespace["goals"] = Goals(self.bridge)
        self.namespace["instances"] = Instances(self.bridge)
        self.namespace["gates"] = ReleaseGates(self.bridge)
        self.namespace["ctx"] = Context(self.bridge)
        self.namespace["preview"] = preview
        self.namespace["console"] = self.console
        agent_class_api.configure(self.bridge, lambda: self.namespace, preview)
        self.namespace["agent"] = agent_class_api.agent
        self.namespace["Agent"] = agent_class_api.Agent
        self.namespace["AgentCallError"] = agent_class_api.AgentCallError
        install_inference(self.namespace, self.bridge)
        # Declared instance state survives reset_scratch; every other name is invocation scratch.
        self.namespace["state"] = {}
        self.bindings = {name: value for name, value in self.namespace.items() if name != "state"}

    def reset_scratch(self) -> None:
        state = self.namespace.get("state", {})
        self.namespace.clear()
        self.namespace.update(self.bindings)
        self.namespace["state"] = state
        self.console.clear()
        _DEFINITIONS.clear()
        # Agent classes and their instances live in `state`; define the classes again for the fresh scratch.
        agent_class_api.rehydrate()

# Fields that identify a frame; never shortened to fit it.
_FRAME_KEYS = frozenset({"event", "id", "status"})


def _shrink_frame_value(value: Any, keep: int) -> Any:
    """Keep the head and tail of every string (in characters) and list (in items) within `keep`."""
    if isinstance(value, str):
        if len(value) <= keep:
            return value
        half = keep // 2
        cut = len(value) - 2 * half
        return f"{value[:half]}\n[... {cut} characters cut to fit the 1 MiB protocol frame ...]\n{value[len(value) - half:]}"
    if isinstance(value, (list, tuple)):
        items = list(value)
        if len(items) > keep:
            half = keep // 2
            items = [*items[:half], f"[... {len(items) - 2 * half} items cut to fit the 1 MiB protocol frame ...]",
                     *items[len(items) - half:]]
        return [_shrink_frame_value(item, keep) for item in items]
    if isinstance(value, dict):
        return {key: _shrink_frame_value(item, keep) for key, item in value.items()}
    return value


def _encode_frame(payload: dict[str, Any]) -> str:
    """One protocol line within the host's 1 MiB frame. The host ends a kernel that sends a larger frame, losing
    every variable, so an oversized output (a large capture budget, many names) is cut here, in the cell."""
    line = json.dumps(payload, default=str, separators=(",", ":"))
    keep = _MAX_HOST_FRAME_BYTES // 2
    while len(line) > _MAX_HOST_FRAME_BYTES and keep > 0:
        # JSON escaping can grow a character up to 12 bytes; halve until the frame fits.
        keep //= 2
        shrunk = {key: value if key in _FRAME_KEYS else _shrink_frame_value(value, keep) for key, value in payload.items()}
        line = json.dumps(shrunk, default=str, separators=(",", ":"))
    if len(line) > _MAX_HOST_FRAME_BYTES:
        # Only values json.dumps renders with str() can still be this large: keep the frame's identity.
        line = json.dumps({key: payload[key] for key in payload if key in _FRAME_KEYS}, separators=(",", ":"))
    return line + "\n"


# Protocol channel. With --protocol-fds (every platform but Windows) frames go to the private fd 3 and host
# messages arrive on fd 4, both close-on-exec, so fd 0/1/2 are plain streams: stdin is /dev/null, and bytes that
# anything writes to fd 1 or fd 2 (os.write, subprocesses, C code) are output the host attributes to the running
# cell rather than protocol. Without the flag (Windows) the protocol stays on stdout and stdin.
_PROTOCOL_FDS = (3, 4)
_PRIVATE_PROTOCOL = False
_PROTOCOL_OUT: Any = None
_PROTOCOL_IN: Any = None
# (st_dev, st_ino) of the pipes fd 1 and fd 2 were at startup; a flush marker goes only to the host's own pipe.
_RAW_STREAMS: dict[str, tuple[int, tuple[int, int]]] = {}
_LIBC_FFLUSH: Any = None


def _open_protocol() -> None:
    global _PRIVATE_PROTOCOL, _PROTOCOL_OUT, _PROTOCOL_IN, _LIBC_FFLUSH
    if "--protocol-fds" not in sys.argv[1:]:
        _PROTOCOL_OUT, _PROTOCOL_IN = sys.__stdout__.buffer, sys.__stdin__.buffer
        return
    out_fd, in_fd = _PROTOCOL_FDS
    # Close-on-exec: no process a cell starts (os.system, subprocess with close_fds=False) can reach the protocol.
    os.set_inheritable(out_fd, False)
    os.set_inheritable(in_fd, False)
    _PROTOCOL_OUT = os.fdopen(out_fd, "wb")
    _PROTOCOL_IN = os.fdopen(in_fd, "rb")
    _PRIVATE_PROTOCOL = True
    for name, fd in (("stdout", 1), ("stderr", 2)):
        try:
            stat = os.fstat(fd)
            _RAW_STREAMS[name] = (fd, (stat.st_dev, stat.st_ino))
        except OSError:
            pass
    try:
        import ctypes
        _LIBC_FFLUSH = ctypes.CDLL(None).fflush
    except Exception:
        _LIBC_FFLUSH = None
    import builtins

    def _no_input(prompt: Any = "") -> str:
        raise EOFError("input() is not available: the RLM kernel has no interactive stdin (it reads /dev/null); "
                       "put the data in the code, a variable or a file instead")

    builtins.input = _no_input


# Frames are written from the cell's thread and from the live-output timer: one line at a time.
_FRAME_LOCK = threading.Lock()


def _write_frame(line: str) -> None:
    with _FRAME_LOCK:
        _PROTOCOL_OUT.write(line.encode("utf-8"))
        _PROTOCOL_OUT.flush()


def emit(event: str, **fields: Any) -> None:
    _write_frame(_encode_frame({"event": event, **fields}))


def _flush_stdio_buffers() -> None:
    for stream in (sys.__stdout__, sys.__stderr__):
        with contextlib.suppress(BaseException):
            stream.flush()
    if _LIBC_FFLUSH is not None:
        with contextlib.suppress(BaseException):
            _LIBC_FFLUSH(None)


def _write_flush_marker(fd: int, identity: tuple[int, int], marker: bytes) -> bool:
    try:
        # Cell code may have closed or redirected the fd: the marker would never reach the host.
        stat = os.fstat(fd)
        if (stat.st_dev, stat.st_ino) != identity:
            return False
        view = memoryview(marker)
        while view:
            view = view[os.write(fd, view) :]
        return True
    except BaseException:
        return False


def _flush_raw_output(request_id: Any) -> dict[str, bool] | None:
    """Write the cell's flush marker to fd 1 and fd 2, after anything buffered in Python or C stdio. The host
    finalizes the cell once it has read each marker written here, so raw output from the cell precedes it."""
    if not _PRIVATE_PROTOCOL:
        return None
    _flush_stdio_buffers()
    marker = f"\x1eultron-rlm-flush:{request_id}\x1e".encode("utf-8")
    flushed: dict[str, bool] = {}
    for name in ("stdout", "stderr"):
        stream_info = _RAW_STREAMS.get(name)
        if stream_info is None:
            flushed[name] = False
        else:
            fd, identity = stream_info
            flushed[name] = _write_flush_marker(fd, identity, marker)
    return flushed


# Name -> (source of the top-level def, class or import statement that bound it, the object it bound), in the
# order they ran. A snapshot keeps the source of each name still bound to that object and a restore runs it again.
_DEFINITIONS: dict[str, tuple[str, Any]] = {}


def _definitions(tree: ast.Module) -> list[tuple[str, str]]:
    found: list[tuple[str, str]] = []
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            found.append((node.name, ast.unparse(node)))
        elif isinstance(node, ast.Import):
            found.extend(
                (alias.asname or alias.name.partition(".")[0], ast.unparse(ast.Import(names=[alias])))
                for alias in node.names
            )
        elif isinstance(node, ast.ImportFrom):
            found.extend(
                (
                    alias.asname or alias.name,
                    ast.unparse(ast.ImportFrom(module=node.module, names=[alias], level=node.level)),
                )
                for alias in node.names
                if alias.name != "*"
            )
    return found


def _prune_stale_definitions() -> None:
    stale = [
        name
        for name, (_, bound) in _DEFINITIONS.items()
        if _STATE.namespace.get(name) is not bound
    ]
    for name in stale:
        _DEFINITIONS.pop(name, None)


def _record_definitions(definitions: list[tuple[str, str]], before: dict[str, Any]) -> None:
    """Keep the statements of a cell that bound a new object; one the cell never reached leaves its name as it was."""
    for name, source in definitions:
        value = _STATE.namespace.get(name)
        if value is not None and value is not before.get(name):
            _DEFINITIONS.pop(name, None)
            _DEFINITIONS[name] = (source, value)
    _prune_stale_definitions()


def _persistable_namespace() -> tuple[dict[str, str], dict[str, str], list[tuple[str, str]]]:
    return _snapshot_persistable(_STATE.namespace, _STATE.bindings, _DEFINITIONS)


def save_snapshot(path: str) -> dict[str, Any]:
    """Write a constrained data-only snapshot: a sha256 header line, then a JSON body, joined from each name's text.

    The digest detects corruption. Authenticity is the host's job: it signs the file with an
    HMAC key this process never sees and verifies it before any restore.
    """
    return save_snapshot_file(path, _STATE.namespace, _STATE.bindings, _DEFINITIONS)


def restore_snapshot(path: str, verified_sha256: Any = None) -> dict[str, Any]:
    """Restore a snapshot the host has already authenticated.

    The host checks the HMAC signature line and passes the sha256 of the signed content; the bytes read
    here must hash to that value, so a file swapped after the host's check is refused as well.
    """
    return restore_snapshot_file(
        path, verified_sha256, _STATE.namespace, _DEFINITIONS, _remember_cell_source
    )


_UNCLOSED = re.compile(r"^'([(\[{])' was never closed$")
_CLOSERS = {"(": ")", "[": "]", "{": "}"}


def _parse_cell(source: str) -> tuple[ast.Module, str, str | None]:
    """Parse a cell; a cell whose only fault is brackets left open at its very end (typically
    ``print(await bash('''...''')`` short of one ``)``) is closed there, with a note for the output."""
    try:
        return ast.parse(source, filename="<rlm-cell>", mode="exec"), source, None
    except SyntaxError as parse_error:
        original = error = parse_error
    added = ""
    for _ in range(3):
        match = _UNCLOSED.match(str(error.msg or ""))
        if match is None:
            break
        added += _CLOSERS[match.group(1)]
        repaired = source.rstrip() + added + "\n"
        try:
            tree = ast.parse(repaired, filename="<rlm-cell>", mode="exec")
        except SyntaxError as retry:
            error = retry
            continue
        return tree, repaired, f"[note: added the missing {added!r} at the end of the cell]"
    raise original


def _string_token_count(segment: str) -> int:
    try:
        return sum(1 for token in tokenize.generate_tokens(io.StringIO(segment).readline) if token.type == tokenize.STRING)
    except (tokenize.TokenError, SyntaxError):
        return -1


_QUOTED_ESCAPE = re.compile(r"\\(?:\\|['\"])")


def _raw_literal_segment(source: str, node: ast.expr) -> tuple[str, str] | None:
    if not isinstance(node, ast.Constant) or not isinstance(node.value, str):
        return None
    segment = ast.get_source_segment(source, node)
    if not segment or "\\" not in segment:
        return None
    prefix = segment[: len(segment) - len(segment.lstrip("rRbBuUfF"))]
    if prefix.lower() not in ("", "u") or _QUOTED_ESCAPE.search(segment):
        return None
    if _string_token_count(segment) != 1:
        return None
    return segment, prefix


def _raw_command_literal(source: str, node: ast.expr) -> str | None:
    """The value of a plain string literal read as if written raw, when that differs.

    A shell command is text for bash, which does its own escaping (and a heredoc's program its own):
    ``bash('''python - <<'PY' ... print('a\\nb') ...''')`` means the two characters ``\\n``, as
    Pi's bash tool would pass them. Python would turn them into a real newline and break the inner
    program, so a literal whose backslashes only make sense raw is read raw. Literals written raw,
    f-strings, concatenations, and ones that already escape backslashes or quotes (``\\\\``,
    ``\\'``) are left as written.
    """
    extracted = _raw_literal_segment(source, node)
    if extracted is None:
        return None
    segment, prefix = extracted
    try:
        raw = ast.literal_eval("r" + segment[len(prefix) :])
    except (SyntaxError, ValueError):
        return None
    return raw if isinstance(raw, str) and raw != node.value else None


class _RawShellLiterals(ast.NodeTransformer):
    def __init__(self, source: str) -> None:
        self._source = source

    def visit_Call(self, node: ast.Call) -> ast.AST:
        self.generic_visit(node)
        if isinstance(node.func, ast.Name) and node.func.id == "bash":
            if node.args:
                raw = _raw_command_literal(self._source, node.args[0])
                if raw is not None:
                    node.args[0] = ast.copy_location(ast.Constant(raw), node.args[0])
            for keyword in node.keywords:
                if keyword.arg == "command":
                    raw = _raw_command_literal(self._source, keyword.value)
                    if raw is not None:
                        keyword.value = ast.copy_location(ast.Constant(raw), keyword.value)
        return node


_PROTECTED_BINDINGS: frozenset[str] = frozenset()


def _prepare_code(source: str) -> tuple[Any, str | None, list[tuple[str, str]]]:
    tree, text, note = _parse_cell(source)
    _check_protected_overwrites(tree, _PROTECTED_BINDINGS)
    tree = _RawShellLiterals(text).visit(tree)
    definitions = _definitions(tree)
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        tree.body[-1] = ast.Assign(
            targets=[ast.Name(id="_rlm_result", ctx=ast.Store())],
            value=tree.body[-1].value,
        )
    ast.fix_missing_locations(tree)
    code = compile(tree, "<rlm-cell>", "exec", flags=ast.PyCF_ALLOW_TOP_LEVEL_AWAIT)
    _remember_cell_source(code, text)
    return code, note, definitions


# Every cell is compiled as "<rlm-cell>", so a traceback cannot find its text by file name; the code objects of
# recent cells (functions they define included) map to their lines instead. A failing line shown in the traceback
# is one the model does not have to find again before fixing it.
_CELL_SOURCES: dict[int, tuple[Any, list[str]]] = {}
_CELL_SOURCE_ORDER: list[list[int]] = []
_CELL_SOURCES_KEPT = 200


def _remember_cell_source(code: Any, text: str) -> None:
    lines = text.splitlines()
    keys: list[int] = []
    pending = [code]
    while pending:
        current = pending.pop()
        _CELL_SOURCES[id(current)] = (current, lines)
        keys.append(id(current))
        pending.extend(const for const in current.co_consts if hasattr(const, "co_code"))
    _CELL_SOURCE_ORDER.append(keys)
    while len(_CELL_SOURCE_ORDER) > _CELL_SOURCES_KEPT:
        for key in _CELL_SOURCE_ORDER.pop(0):
            _CELL_SOURCES.pop(key, None)


def _frame_source_line(code: Any, lineno: int | None) -> str:
    if not lineno:
        return ""
    entry = _CELL_SOURCES.get(id(code))
    if entry is not None and entry[0] is code:
        lines = entry[1]
        return lines[lineno - 1].strip() if 0 < lineno <= len(lines) else ""
    import linecache

    with contextlib.suppress(Exception):
        return linecache.getline(code.co_filename, lineno).strip()
    return ""


def _output_budget() -> int:
    """Bytes of a cell's stdout or stderr kept for the model (ULTRON_RLM_OUTPUT_BYTES, default 20 KB)."""
    raw = os.environ.get("ULTRON_RLM_OUTPUT_BYTES", "")
    try:
        value = int(raw)
    except ValueError:
        return 20_000
    return value if value > 0 else 20_000


# Bytes of one stream kept in its spill file, and how many spill files a kernel keeps.
_SPILL_MAX_BYTES = 64 * 1024 * 1024
_LAST_OUTPUT_MAX_BYTES = 16 * 1024 * 1024
_SPILL_KEEP = 20
_SPILL_CHUNK = 1024 * 1024


def _spill_dir() -> str | None:
    """Where a cell's cut output is saved (ULTRON_RLM_SPILL_DIR, set by the session worker); None: not saved."""
    value = os.environ.get("ULTRON_RLM_SPILL_DIR", "").strip()
    return value or None


def _open_spill(directory: str, name: str):
    """A new private file (0600 in a 0700 directory) for a stream's full text; the oldest beyond _SPILL_KEEP go."""
    os.makedirs(directory, mode=0o700, exist_ok=True)
    os.chmod(directory, 0o700)
    with contextlib.suppress(OSError):
        files = sorted(
            (entry for entry in os.scandir(directory) if entry.is_file() and entry.name.endswith(".txt")),
            key=lambda entry: entry.stat().st_mtime_ns,
        )
        for entry in files[: max(0, len(files) - (_SPILL_KEEP - 1))]:
            os.unlink(entry.path)
    path = os.path.join(directory, f"{time.time_ns()}-{os.getpid()}-{name}.txt")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    return path, os.fdopen(fd, "wb")


class _MiddleTextIO(io.TextIOBase):
    """Capture a stream keeping its head and tail (each half the budget) and count what was cut,
    so both the first error and the final summary of a long output survive (nano-rlm style).
    With a spill directory, a stream that outgrows the budget is also written whole (up to
    _SPILL_MAX_BYTES) to a private file named in the truncation marker."""

    def __init__(self, limit: int, name: str = "output", spill_dir: str | None = None) -> None:
        super().__init__()
        self._half = max(1, limit // 2)
        self._head = bytearray()
        self._tail = bytearray()
        self._total = 0
        self._name = name
        self._spill_dir = spill_dir
        self._spill = None
        self._spill_path: str | None = None
        self._spilled = 0

    def writable(self) -> bool:
        return True

    def _spill_bytes(self, data: bytes) -> None:
        room = _SPILL_MAX_BYTES - self._spilled
        if self._spill is None or room <= 0 or not data:
            return
        try:
            self._spill.write(data[:room])
        except OSError:
            self._close_spill()
            self._spill_path = None
            return
        self._spilled += min(len(data), room)

    def _start_spill(self) -> None:
        """Open the spill file once the stream outgrows the budget; until then head and tail hold every byte."""
        if self._spill_dir is None or self._spill_path is not None:
            return
        directory, self._spill_dir = self._spill_dir, None
        try:
            self._spill_path, self._spill = _open_spill(directory, self._name)
        except OSError:
            return
        self._spill_bytes(bytes(self._head) + bytes(self._tail))

    def _close_spill(self) -> None:
        if self._spill is not None:
            with contextlib.suppress(OSError):
                self._spill.close()
            self._spill = None

    def _write_huge_text(self, text: str) -> int:
        length = len(text)
        self._start_spill()
        for offset in range(0, length, _SPILL_CHUNK):
            if self._spill is None or self._spilled >= _SPILL_MAX_BYTES:
                break
            chunk = text[offset : offset + _SPILL_CHUNK].encode(
                "utf-8", errors="backslashreplace"
            )
            self._spill_bytes(chunk)
        head_room = max(0, self._half - len(self._head))
        self._total += len(text.encode("utf-8", errors="backslashreplace"))
        front = text[:head_room].encode("utf-8", errors="backslashreplace")[:head_room]
        self._head.extend(front)
        tail_bytes = text[-self._half :].encode("utf-8", errors="backslashreplace")
        self._tail = bytearray(tail_bytes[-self._half :])
        return length

    def _append_tail(self, encoded: bytes) -> None:
        if not encoded:
            return
        self._tail.extend(encoded)
        if len(self._tail) > 2 * self._half:
            del self._tail[: len(self._tail) - self._half]

    def write(self, text: str) -> int:
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not isinstance(text, str):
            raise TypeError("write() argument must be str")
        # Bound the temporary encoding of a huge write: only its ends can be kept.
        if len(text) > 4 * self._half + 8:
            return self._write_huge_text(text)
        encoded = text.encode("utf-8", errors="backslashreplace")
        if self._total + len(encoded) > 2 * self._half:
            self._start_spill()
        self._spill_bytes(encoded)
        self._total += len(encoded)
        head_room = self._half - len(self._head)
        if head_room > 0:
            self._head.extend(encoded[:head_room])
            encoded = encoded[head_room:]
        self._append_tail(encoded)
        return len(text)

    def getvalue(self) -> str:
        self._close_spill()
        tail = bytes(self._tail[-self._half:])
        kept = len(self._head) + len(tail)
        head = self._head.decode("utf-8", errors="ignore")
        if kept >= self._total:
            return head + tail.decode("utf-8", errors="ignore")
        where = ""
        if self._spill_path is not None:
            whole = "full" if self._spilled >= self._total else f"first {self._spilled} bytes of"
            where = f"; {whole} {self._name} in {self._spill_path}"
        # Drop partial code points at the cut rather than add replacement glyphs.
        return (
            head
            + f"\n[... {self._total - kept} bytes truncated{where} ...]\n"
            + tail.decode("utf-8", errors="ignore")
        )


def _error_preview(error: BaseException) -> dict[str, Any]:
    return render_error_preview(
        error,
        skip_file=__file__,
        frame_source_line=_frame_source_line,
    )


_STATE = RuntimeState()

install_lanes(RLMNamespace)
install_extensions(_STATE.namespace, _STATE.bridge)
install_swarm(RLMNamespace, _STATE.namespace, _STATE.bridge)
install_images(RLMNamespace)
_STATE.namespace["schedule"] = Schedule(_STATE.bridge)
_STATE.namespace["diagram"] = Diagram(_STATE.bridge)
_STATE.namespace["sessions"] = Sessions(_STATE.bridge)
_STATE.namespace["judge"] = Judge(_STATE.bridge)
_STATE.namespace["knowledge"] = KnowledgeApi(_STATE.bridge)
_STATE.bindings = {name: value for name, value in _STATE.namespace.items() if name != "state"}
_PROTECTED_BINDINGS = frozenset(name for name in _STATE.bindings if name in _CORE_HOST_BINDINGS)


def _in_cell_frame(frame: Any) -> bool:
    cur = frame
    while cur is not None:
        code = cur.f_code
        if code.co_filename == "<rlm-cell>" or code is execute_cell.__code__:
            return True
        cur = cur.f_back
    return False


def _on_interrupt(_signum: int, frame: Any) -> None:
    if not _STATE.cell_active:
        return
    if _in_cell_frame(frame):
        raise KeyboardInterrupt("cell interrupted")
    task = _STATE.active_task
    if task is not None and not task.done():
        task.cancel()


def _open_cell_streams(
    request_id: str,
) -> tuple[_MiddleTextIO, _MiddleTextIO, _LiveOutput | None, _CellStream, _CellStream]:
    budget = _output_budget()
    spill_dir = _spill_dir()
    stdout = _MiddleTextIO(budget, "stdout", spill_dir)
    stderr = _MiddleTextIO(budget, "stderr", spill_dir)
    # With a host that keeps partial output (ULTRON_RLM_STREAM_OUTPUT), printed text reaches it live.
    live = _LiveOutput.open(request_id)
    out_tee = _ConsoleTee(_LiveOutput.tee(live, stdout, "stdout"), _STATE.console, "stdout")
    err_tee = _ConsoleTee(_LiveOutput.tee(live, stderr, "stderr"), _STATE.console, "stderr")
    return stdout, stderr, live, _CellStream(request_id, out_tee), _CellStream(request_id, err_tee)


def _finalize_cell_result() -> str:
    if "_rlm_result" not in _STATE.namespace:
        return ""
    value = _STATE.namespace.pop("_rlm_result")
    if value is None:
        # As in IPython, a None expression (a bare print(...) call) shows nothing.
        return ""
    # Keep the value reachable as `_`, so a by-reference preview can be followed up.
    _STATE.namespace["_"] = value
    with _preview_deadline():
        return _cell_result_text(value)


def _bind_spilled_output(stdout: _MiddleTextIO) -> None:
    """Bind spilled cell stdout as a ContextHandle (`last_output`) so the next cell can grep/slice it in Python."""
    path = stdout._spill_path
    if not path:
        return
    with contextlib.suppress(OSError):
        if os.path.getsize(path) > _LAST_OUTPUT_MAX_BYTES:
            return
        raw = Path(path).read_bytes()
        text = raw.decode("utf-8", errors="replace")
        digest = hashlib.sha256(raw).hexdigest()
        _STATE.namespace["last_output"] = ContextHandle(text, digest, "last_output", len(raw))


def _emit_cell_error(
    request_id: str,
    live: _LiveOutput | None,
    stdout: _MiddleTextIO,
    stderr: _MiddleTextIO,
    error: BaseException,
) -> None:
    _STATE.cell_active = False
    _LiveOutput.close(live)
    try:
        emit("stdout", id=request_id, text=stdout.getvalue())
        _bind_spilled_output(stdout)
        emit("stderr", id=request_id, text=stderr.getvalue())
        try:
            with _preview_deadline():
                details = _error_preview(error)
        except BaseException:
            details = {
                "ename": _text_preview(type(error).__name__),
                "evalue": "<error preview failed>",
                "traceback": [],
            }
        if isinstance(error, MemoryError) and _MAX_MEMORY_MB > 0:
            note = _memory_limit_note()
            details["evalue"] = f"{details['evalue']} ({note})" if details["evalue"] else note
        emit("error", id=request_id, **details)
        emit("done", id=request_id, status="error", flush=_flush_raw_output(request_id))
    except BaseException as secondary:
        # Exit with a mapped code when the failure cannot be reported in-band.
        _exit_for(error, secondary)


async def execute_cell(request_id: str, source: str) -> None:
    async with _STATE.execution_lock:
        _CELL.set(request_id)
        _STATE.active_task = asyncio.current_task()
        _STATE.active_request_id = request_id
        stdout, stderr, live, out, err = _open_cell_streams(request_id)
        _STATE.namespace.pop("_rlm_result", None)
        try:
            compiled, note, definitions = _prepare_code(source)
            bound_before = {name: _STATE.namespace.get(name) for name, _ in definitions}
            if note is not None:
                stdout.write(note + "\n")
            _arm_cpu_limit()
            _STATE.cell_active = True
            try:
                with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                    agent_class_api.begin_cell(source)
                    try:
                        result = eval(compiled, _STATE.namespace, _STATE.namespace)
                        if asyncio.iscoroutine(result):
                            await result
                    finally:
                        agent_class_api.end_cell()
                        _record_definitions(definitions, bound_before)
                result_text = _finalize_cell_result()
            finally:
                _STATE.cell_active = False
                _LiveOutput.close(live)
            dropped = out.dropped + err.dropped
            if dropped:
                stderr.write(f"[{dropped} characters printed by a task of an earlier cell were dropped]\n")
            emit("stdout", id=request_id, text=stdout.getvalue())
            _bind_spilled_output(stdout)
            emit("stderr", id=request_id, text=stderr.getvalue())
            emit("result", id=request_id, result=result_text)
            emit("done", id=request_id, status="ok", flush=_flush_raw_output(request_id))
        except BaseException as error:
            _emit_cell_error(request_id, live, stdout, stderr, error)
        finally:
            if _STATE.active_request_id == request_id:
                _STATE.active_task = None
                _STATE.active_request_id = None


def _exit_for(*errors: BaseException) -> None:
    code = _EXIT_INTERNAL
    if any(isinstance(error, MemoryError) for error in errors):
        code = _EXIT_MEMORY
    elif any(isinstance(error, RlmCpuLimitExceeded) for error in errors):
        code = _EXIT_CPU
    for stream in (_PROTOCOL_OUT, sys.__stdout__, sys.__stderr__):
        try:
            stream.flush()
        except BaseException:
            pass
    os._exit(code)


def _reset_scratch_op() -> dict[str, Any]:
    _STATE.reset_scratch()
    return {}


# Ops besides execute: each runs under the execution lock, like a cell, as a task of its own, so the reader loop keeps
# reading host replies while it waits, and an op never runs beside a cell or another op.
_OPS = {
    "snapshot": lambda frame: {"snapshot": save_snapshot(str(frame.get("path")))},
    "restore": lambda frame: {"restore": restore_snapshot(str(frame.get("path")), frame.get("content_sha256"))},
    "reset_scratch": lambda _frame: _reset_scratch_op(),
}


async def _run_op(request_id: Any, request_type: str, frame: dict[str, Any]) -> None:
    async with _STATE.execution_lock:
        try:
            fields = _OPS[request_type](frame)
        except Exception as error:
            emit("done", id=request_id, status="error", error=_exception_message(error))
            return
        emit("done", id=request_id, status="ok", **fields)


def _interrupt_active_cell(request_id: Any) -> None:
    task = _STATE.active_task
    matches_request = request_id is None or _STATE.active_request_id == request_id
    if task is not None and not task.done() and matches_request:
        task.cancel()


def _reject_unknown_request(request_id: Any, request_type: Any) -> None:
    detail = (
        _text_preview(request_type)
        if isinstance(request_type, str)
        else _result_preview(request_type)
    )
    emit("done", id=request_id, status="error", error=_text_preview("unknown request: " + detail))


async def handle_request(frame: dict[str, Any]) -> None:
    request_type = frame.get("request")
    request_id = frame.get("id")
    if request_type == "execute":
        asyncio.create_task(execute_cell(request_id, str(frame.get("code", ""))))
    elif request_type == "interrupt":
        _interrupt_active_cell(request_id)
    elif request_type == "host_reply":
        _STATE.bridge.resolve(str(frame.get("id")), frame.get("payload"), frame.get("error"))
    elif isinstance(request_type, str) and request_type in _OPS:
        asyncio.create_task(_run_op(request_id, request_type, frame))
    else:
        _reject_unknown_request(request_id, request_type)


async def main() -> None:
    if hasattr(signal, "SIGINT"):
        signal.signal(signal.SIGINT, _on_interrupt)
    # Output before this marker (interpreter warnings, for example) is startup diagnostics, not a cell's output.
    _flush_raw_output("ready")
    emit("ready", protocol=1, pid=os.getpid())
    while True:
        line = await asyncio.to_thread(_PROTOCOL_IN.readline)
        if not line:
            return
        request_id = None
        try:
            frame = json.loads(line.decode("utf-8"))
            if isinstance(frame, dict):
                request_id = frame.get("id")
                await handle_request(frame)
        except SystemExit:
            return
        except Exception as error:
            # The host ends a request only by its id: a failure it can attribute is that request's error.
            if isinstance(request_id, str) and request_id:
                emit("done", id=request_id, status="error", error=_exception_message(error))
            else:
                emit("error", **_error_preview(error))


# How often a running cell's new output is sent, and the most one send carries (the newest bytes; older ones are
# replaced by a marker). The host keeps a bounded tail of it as the cell's durable partial output.
_LIVE_INTERVAL = 0.2
_LIVE_MAX_CHARS = 16 * 1024


def _live_output_enabled() -> bool:
    value = os.environ.get("ULTRON_RLM_STREAM_OUTPUT", "").strip().lower()
    return value not in ("", "0", "off", "false", "no")


class _LiveOutput:
    """A running cell's printed output, sent to the host as it is written (`cell.output` host requests that nothing
    waits on), at most every _LIVE_INTERVAL seconds from a timer thread, so output before a long blocking call is
    sent too. The cell's own result still carries its whole captured output."""

    def __init__(self, request_id: str) -> None:
        self._request_id = request_id
        self._lock = threading.Lock()
        self._pending: list[str] = []
        self._size = 0
        self._dropped = 0
        self._timer: threading.Timer | None = None
        self._closed = False
        self._sent = 0

    @staticmethod
    def open(request_id: str) -> "_LiveOutput | None":
        return _LiveOutput(request_id) if _live_output_enabled() and _PROTOCOL_OUT is not None else None

    @staticmethod
    def tee(live: "_LiveOutput | None", capture: io.TextIOBase, name: str) -> io.TextIOBase:
        return capture if live is None else _LiveTee(capture, live)

    @staticmethod
    def close(live: "_LiveOutput | None") -> None:
        if live is not None:
            live._close()

    def add(self, text: str) -> None:
        if not text:
            return
        with self._lock:
            if self._closed:
                return
            self._pending.append(text)
            self._size += len(text)
            while self._size > _LIVE_MAX_CHARS and len(self._pending) > 1:
                dropped = self._pending.pop(0)
                self._size -= len(dropped)
                self._dropped += len(dropped)
            if self._timer is None:
                self._timer = threading.Timer(_LIVE_INTERVAL, self._flush)
                self._timer.daemon = True
                self._timer.start()

    def _flush(self) -> None:
        with self._lock:
            self._timer = None
            if self._closed or not self._pending:
                return
            text = "".join(self._pending)
            dropped = self._dropped + max(0, len(text) - _LIVE_MAX_CHARS)
            text = text[-_LIVE_MAX_CHARS:]
            self._pending, self._size, self._dropped = [], 0, 0
            self._sent += 1
            frame_id = f"out-{self._request_id}-{self._sent}"
        if dropped:
            text = f"[... {dropped} characters not sent ...]\n" + text
        payload = {"text": text.encode("utf-8", errors="backslashreplace").decode("utf-8")}
        try:
            _write_frame(_encode_frame({"event": "host_request", "id": frame_id, "cell": self._request_id,
                                        "type": "cell.output", "payload": payload}))
        except BaseException:
            pass

    def _close(self) -> None:
        with self._lock:
            self._closed = True
            timer, self._timer = self._timer, None
            self._pending = []
        if timer is not None:
            timer.cancel()


class _CellStream(io.TextIOBase):
    """sys.stdout or sys.stderr while a cell runs. The redirect is process-wide, so a task an earlier cell left running
    would print into this cell's output; text from a context that names another cell is dropped and counted instead.
    Text from a context without a cell (a plain thread) is this cell's, as before."""

    def __init__(self, cell: str, target: io.TextIOBase) -> None:
        super().__init__()
        self._cell = cell
        self._target = target
        self.dropped = 0

    def writable(self) -> bool:
        return True

    def write(self, text: str) -> int:
        owner = _CELL.get()
        if owner is not None and owner != self._cell:
            self.dropped += len(text)
            return len(text)
        return self._target.write(text)

    def flush(self) -> None:
        if not getattr(self._target, "closed", False):
            self._target.flush()

    def getvalue(self) -> str:
        return self._target.getvalue()  # type: ignore[attr-defined]


class _LiveTee(io.TextIOBase):
    """Writes to the cell's capture and to its live output."""

    def __init__(self, capture: io.TextIOBase, live: _LiveOutput) -> None:
        super().__init__()
        self._capture = capture
        self._live = live

    def writable(self) -> bool:
        return True

    def write(self, text: str) -> int:
        written = self._capture.write(text)
        self._live.add(text)
        return written

    def getvalue(self) -> str:
        return self._capture.getvalue()  # type: ignore[attr-defined]


# Notes on a bash result from the host: `exit_note` when exit code 1 reports a result rather than a failure (grep
# found no match: "[No matches found (exit code 1)]" replaces "[exit code 1]"; .ok stays False, as it means a clean
# exit 0), and `sandbox_note`, a line saying the sandbox refused the command and what it allows.
def _bash_notes(result: Any) -> dict[str, str | None]:
    def note(key: str) -> str | None:
        value = result.get(key) if isinstance(result, dict) else None
        return value if isinstance(value, str) and value else None

    return {"exit_note": note("exit_note"), "sandbox_note": note("sandbox_note")}


def _exit_words(exit_code: int | None, exit_note: str | None) -> str:
    return f"{exit_note} (exit code {exit_code})" if exit_note else f"exit code {exit_code}"


def _bash_exit_lines(exit_code: int | None, notes: dict[str, Any]) -> str:
    sandbox_note = notes.get("sandbox_note")
    return (f"\n{sandbox_note}" if sandbox_note else "") + f"\n[{_exit_words(exit_code, notes.get('exit_note'))}]"


def _matching_models(result: Any, query: str, limit: int) -> list[dict[str, Any]]:
    """The host's model list filtered to those whose provider, id or name holds every word of `query`."""
    words = query.lower().split()
    # ponytail: catalog order, no ranking; rank by match quality if callers need best-first.
    found = [
        m for m in (result if isinstance(result, list) else [])
        if isinstance(m, dict)
        and all(w in " ".join(str(m.get(k, "")) for k in ("provider", "id", "name")).lower() for w in words)
    ]
    return found[:max(limit, 0)]


async def _race_models(self: RLMNamespace) -> list[str]:
    """Models the user picked in /settings for subagent races (arena, swarm model races); [] when none."""
    result = await self._bridge.request("rlm.race_models", {})
    return [m for m in result if isinstance(m, str)] if isinstance(result, list) else []


RLMNamespace.race_models = _race_models


if __name__ == "__main__":
    _open_protocol()
    _become_subreaper()
    _apply_resource_limits()
    try:
        asyncio.run(main())
    except (MemoryError, RlmCpuLimitExceeded) as fatal:
        _exit_for(fatal)
