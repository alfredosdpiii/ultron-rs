"""Bounded console buffer and AST pre-flight guard against overwriting protected REPL host bindings."""
from __future__ import annotations

import ast
import collections
import contextlib
import io
import re
import threading
from typing import Any


_CONSOLE_STREAMS = frozenset(("all", "stdout", "stderr", "log"))
_CONSOLE_DEFAULT_MAX_BYTES = 256 * 1024
_CONSOLE_DEFAULT_MAX_LINES = 4000


class _ConsoleWriter(io.TextIOBase):
    """Stream writer that feeds a ConsoleBuffer without emitting to the cell's model-facing output."""

    def __init__(self, buf: "ConsoleBuffer", stream: str) -> None:
        super().__init__()
        self._buf = buf
        self._stream = stream

    def writable(self) -> bool:
        return True

    def write(self, text: str) -> int:
        if not isinstance(text, str):
            raise TypeError("write() argument must be str")
        self._buf.record(self._stream, text)
        return len(text)


class _ConsoleTee(io.TextIOBase):
    """Tees cell stdout/stderr into both the cell's capture stream and the session's ConsoleBuffer."""

    def __init__(self, target: io.TextIOBase, buf: "ConsoleBuffer", stream: str) -> None:
        super().__init__()
        self._target = target
        self._buf = buf
        self._stream = stream

    def writable(self) -> bool:
        return True

    def write(self, text: str) -> int:
        written = self._target.write(text)
        self._buf.record(self._stream, text)
        return written

    def flush(self) -> None:
        if not getattr(self._target, "closed", False):
            self._target.flush()

    def getvalue(self) -> str:
        return self._target.getvalue()  # type: ignore[attr-defined]


class ConsoleBuffer:
    """Bounded, queryable stdout/stderr/log ring buffer that persists across cells.

    Normal `print()` output is still returned by the cell and also recorded here so later cells can inspect
    it with `console.tail(n)`, `console.head(n)`, `console.search(pattern)` or `console.preview()`.
    `console.log(*args)` and `with console.capture(): ...` write ONLY to this buffer, keeping noisy output
    out of model context unless explicitly queried.
    """

    def __init__(self, max_bytes: int = _CONSOLE_DEFAULT_MAX_BYTES, max_lines: int = _CONSOLE_DEFAULT_MAX_LINES) -> None:
        self._max_bytes = max(256, int(max_bytes))
        self._max_lines = max(1, int(max_lines))
        self._lock = threading.Lock()
        self._lines: collections.deque[tuple[str, str, int]] = collections.deque()
        self._partial: dict[str, str] = {}
        self._bytes = 0
        self._dropped_lines = 0
        self._dropped_bytes = 0

    def _evict_locked(self) -> None:
        while self._lines and (self._bytes > self._max_bytes or len(self._lines) > self._max_lines):
            _stream, _line, size = self._lines.popleft()
            self._bytes -= size
            self._dropped_lines += 1
            self._dropped_bytes += size

    def _append_line_locked(self, stream: str, line: str) -> None:
        raw = line.encode("utf-8", errors="backslashreplace")
        if len(raw) > self._max_bytes:
            self._dropped_bytes += len(raw) - self._max_bytes
            line = raw[-self._max_bytes:].decode("utf-8", errors="ignore")
            size = len(line.encode("utf-8"))
        else:
            size = len(raw)
        self._lines.append((stream, line, size))
        self._bytes += size
        self._evict_locked()

    def record(self, stream: str, text: str) -> None:
        if not text:
            return
        with self._lock:
            combined = self._partial.pop(stream, "") + text
            parts = combined.split("\n")
            for line in parts[:-1]:
                self._append_line_locked(stream, line)
            rem = parts[-1]
            if rem:
                raw = rem.encode("utf-8", errors="backslashreplace")
                if len(raw) > self._max_bytes:
                    self._dropped_bytes += len(raw) - self._max_bytes
                    rem = raw[-self._max_bytes:].decode("utf-8", errors="ignore")
                self._partial[stream] = rem

    def log(self, *args: Any, sep: str = " ", end: str = "\n") -> None:
        """Write to the bounded console buffer ('log' stream) without emitting to model-facing stdout."""
        self.record("log", sep.join(str(arg) for arg in args) + end)

    @contextlib.contextmanager
    def capture(self):
        """Redirect sys.stdout and sys.stderr into this buffer only, suppressing model-facing cell output."""
        with contextlib.redirect_stdout(_ConsoleWriter(self, "stdout")), contextlib.redirect_stderr(
            _ConsoleWriter(self, "stderr")
        ):
            yield self

    def _select_locked(self, stream: str) -> list[str]:
        if stream not in _CONSOLE_STREAMS:
            raise ValueError(f"console stream must be one of {sorted(_CONSOLE_STREAMS)}, not {stream!r}")
        out = [line for st, line, _ in self._lines if stream == "all" or st == stream]
        if stream == "all":
            out.extend(self._partial.values())
        elif stream in self._partial:
            out.append(self._partial[stream])
        return out

    def tail(self, n: int = 20, *, stream: str = "all") -> str:
        """Return the last `n` retained lines from `stream` ('all', 'stdout', 'stderr', 'log')."""
        if type(n) is not int or n < 0:
            raise ValueError("console.tail n must be a non-negative integer")
        with self._lock:
            lines = self._select_locked(stream)
        return "\n".join(lines[-n:]) if n > 0 else ""

    def head(self, n: int = 20, *, stream: str = "all") -> str:
        """Return the first `n` retained lines from `stream` ('all', 'stdout', 'stderr', 'log')."""
        if type(n) is not int or n < 0:
            raise ValueError("console.head n must be a non-negative integer")
        with self._lock:
            lines = self._select_locked(stream)
        return "\n".join(lines[:n]) if n > 0 else ""

    def search(self, pattern: str, *, limit: int = 20, stream: str = "all") -> list[str]:
        """Return up to `limit` retained lines matching regex `pattern`."""
        if not isinstance(pattern, str):
            raise TypeError("console.search pattern must be a str")
        if type(limit) is not int or limit < 0:
            raise ValueError("console.search limit must be a non-negative integer")
        regex = re.compile(pattern)
        with self._lock:
            lines = self._select_locked(stream)
        matches: list[str] = []
        for line in lines:
            if regex.search(line):
                matches.append(line)
                if len(matches) >= limit:
                    break
        return matches

    def preview(self, max_bytes: int = 2000, *, stream: str = "all") -> dict[str, Any]:
        """Bounded head/tail summary of retained console output without dumping the whole buffer."""
        if type(max_bytes) is not int or not 64 <= max_bytes <= 1024 * 1024:
            raise ValueError("console.preview max_bytes must be an integer between 64 and 1048576")
        with self._lock:
            lines = self._select_locked(stream)
            dropped_lines = self._dropped_lines
            dropped_bytes = self._dropped_bytes
        text = "\n".join(lines)
        raw = text.encode("utf-8", errors="backslashreplace")
        truncated = len(raw) > max_bytes
        if truncated:
            half = max(1, max_bytes // 2)
            head = raw[:half].decode("utf-8", errors="ignore")
            tail = raw[-half:].decode("utf-8", errors="ignore")
            shown = f"{head}\n[... {len(raw) - 2 * half} bytes elided ...]\n{tail}"
        else:
            shown = text
        return {
            "stream": stream,
            "lines": len(lines),
            "bytes": len(raw),
            "dropped_lines": dropped_lines,
            "dropped_bytes": dropped_bytes,
            "truncated": truncated,
            "preview": shown,
        }

    def stats(self) -> dict[str, int]:
        with self._lock:
            partial_lines = len(self._partial)
            partial_bytes = sum(len(s.encode("utf-8", errors="backslashreplace")) for s in self._partial.values())
            return {
                "lines": len(self._lines) + partial_lines,
                "bytes": self._bytes + partial_bytes,
                "dropped_lines": self._dropped_lines,
                "dropped_bytes": self._dropped_bytes,
                "max_bytes": self._max_bytes,
                "max_lines": self._max_lines,
            }

    def clear(self) -> None:
        with self._lock:
            self._lines.clear()
            self._partial.clear()
            self._bytes = 0
            self._dropped_lines = 0
            self._dropped_bytes = 0


class ReplGlobalOverwrite(RuntimeError):
    """Raised during AST pre-flight when a cell attempts to overwrite or delete a protected host binding."""


# Core host-provided REPL bindings that cells must not shadow or delete; stdlib convenience imports and
# secondary/optional namespaces (such as `notes`) remain rebindable as ordinary cell variables.
_CORE_HOST_BINDINGS = frozenset(
    {
        "rlm",
        "bash",
        "read",
        "edit",
        "write",
        "view_image",
        "preview",
        "console",
        "tools",
        "mcp",
        "ctx",
        "agents",
        "workflows",
        "background",
        "skills",
        "memory",
        "hints",
        "agent",
        "Agent",
        "AgentCallError",
        "agent_message",
        "Budget",
        "ToolCall",
        "ToolResult",
        "ToolError",
        "McpError",
        "SpawnHandle",
        "ShellJob",
    }
)
_PROTECTED_BINDINGS: frozenset[str] = frozenset()


def _reject_overwrite(name: str, node: ast.AST) -> None:
    line = getattr(node, "lineno", 1)
    raise ReplGlobalOverwrite(
        f"cannot overwrite protected host binding {name!r} (line {line}); choose a different variable name"
    )


def _check_import_binding(
    node: ast.Import | ast.ImportFrom,
    forbidden: set[str] | frozenset[str],
) -> None:
    if isinstance(node, ast.Import):
        for alias in node.names:
            bound = alias.asname or alias.name.partition(".")[0]
            if bound in forbidden:
                _reject_overwrite(bound, node)
        return
    for alias in node.names:
        if alias.name == "*":
            continue
        bound = alias.asname or alias.name
        if bound in forbidden:
            _reject_overwrite(bound, node)


def _check_direct_binding(
    node: ast.AST,
    forbidden: set[str] | frozenset[str],
) -> None:
    if isinstance(node, ast.Name):
        if isinstance(node.ctx, (ast.Store, ast.Del)) and node.id in forbidden:
            _reject_overwrite(node.id, node)
    elif isinstance(node, (ast.Import, ast.ImportFrom)):
        _check_import_binding(node, forbidden)
    elif isinstance(node, ast.ExceptHandler) and node.name in forbidden:
        _reject_overwrite(node.name, node)


def _check_match_binding(node: ast.AST, protected: frozenset[str]) -> None:
    for attr_name, field_name in (
        ("MatchAs", "name"),
        ("MatchStar", "name"),
        ("MatchMapping", "rest"),
    ):
        node_cls = getattr(ast, attr_name, None)
        if node_cls is not None and isinstance(node, node_cls):
            bound = getattr(node, field_name, None)
            if bound in protected:
                _reject_overwrite(bound, node)


def _walk_function_node(
    node: ast.FunctionDef | ast.AsyncFunctionDef,
    protected: frozenset[str],
    in_comp: bool,
    walk_fn: Any,
) -> None:
    if not in_comp and node.name in protected:
        _reject_overwrite(node.name, node)
    for dec in node.decorator_list:
        walk_fn(dec, in_comp)
    for default in (*node.args.defaults, *node.args.kw_defaults):
        if default is not None:
            walk_fn(default, in_comp)
    if not in_comp:
        _check_function_globals(node, protected)


def _walk_class_node(
    node: ast.ClassDef,
    protected: frozenset[str],
    in_comp: bool,
    walk_fn: Any,
) -> None:
    if not in_comp and node.name in protected:
        _reject_overwrite(node.name, node)
    for expr in (*node.bases, *node.decorator_list):
        walk_fn(expr, in_comp)
    for kw in node.keywords:
        walk_fn(kw.value, in_comp)


def _walk_lambda_node(node: ast.Lambda, in_comp: bool, walk_fn: Any) -> None:
    for default in (*node.args.defaults, *node.args.kw_defaults):
        if default is not None:
            walk_fn(default, in_comp)


def _walk_comprehension(
    node: ast.ListComp | ast.SetComp | ast.DictComp | ast.GeneratorExp,
    walk_fn: Any,
) -> None:
    # Comprehension loop targets are local; walrus (NamedExpr) inside binds in the enclosing scope.
    for gen in node.generators:
        walk_fn(gen.iter, True)
        for cond in gen.ifs:
            walk_fn(cond, True)
    if isinstance(node, ast.DictComp):
        walk_fn(node.key, True)
        walk_fn(node.value, True)
    else:
        walk_fn(node.elt, True)


def _walk_named_expr(
    node: ast.NamedExpr,
    protected: frozenset[str],
    in_comp: bool,
    walk_fn: Any,
) -> None:
    if isinstance(node.target, ast.Name) and node.target.id in protected:
        _reject_overwrite(node.target.id, node)
    walk_fn(node.value, in_comp)


def _check_function_globals(fn: ast.FunctionDef | ast.AsyncFunctionDef, protected: frozenset[str]) -> None:
    """Reject `global <protected>` inside a function when that function binds or deletes the name."""
    declared: set[str] = set()

    def collect_globals(node: ast.AST) -> None:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            return
        if isinstance(node, ast.Global):
            for name in node.names:
                if name in protected:
                    declared.add(name)
            return
        for child in ast.iter_child_nodes(node):
            collect_globals(child)

    for stmt in fn.body:
        collect_globals(stmt)
    if not declared:
        return

    def check_mutations(node: ast.AST) -> None:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            if node.name in declared:
                _reject_overwrite(node.name, node)
            return
        _check_direct_binding(node, declared)
        for child in ast.iter_child_nodes(node):
            check_mutations(child)

    for stmt in fn.body:
        check_mutations(stmt)


def _check_protected_overwrites(tree: ast.Module, protected: frozenset[str]) -> None:
    """Pre-flight AST check blocking cell overwrites or deletions of host bindings."""
    if not protected:
        return

    def walk(node: ast.AST, in_comp: bool = False) -> None:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            _walk_function_node(node, protected, in_comp, walk)
            return

        if isinstance(node, ast.ClassDef):
            _walk_class_node(node, protected, in_comp, walk)
            return

        if isinstance(node, ast.Lambda):
            _walk_lambda_node(node, in_comp, walk)
            return

        if isinstance(node, (ast.ListComp, ast.SetComp, ast.DictComp, ast.GeneratorExp)):
            _walk_comprehension(node, walk)
            return

        if isinstance(node, ast.NamedExpr):
            _walk_named_expr(node, protected, in_comp, walk)
            return

        if not in_comp:
            _check_direct_binding(node, protected)
            _check_match_binding(node, protected)

        for child in ast.iter_child_nodes(node):
            walk(child, in_comp)

    for stmt in tree.body:
        walk(stmt, False)


