"""Bounded cell stream capture, spill files, live output streaming, and per-cell stdout isolation."""
from __future__ import annotations

import contextlib
import contextvars
import hashlib
import io
import os
import threading
import time
from pathlib import Path
from typing import Any, Callable

from infer_api import ContextHandle

_SPILL_MAX_BYTES = 64 * 1024 * 1024
_LAST_OUTPUT_MAX_BYTES = 16 * 1024 * 1024
_SPILL_KEEP = 20
_SPILL_CHUNK = 1024 * 1024

_LIVE_INTERVAL = 0.2
_LIVE_MAX_CHARS = 16 * 1024


def _output_budget() -> int:
    """Bytes of a cell's stdout or stderr kept for the model (ULTRON_RLM_OUTPUT_BYTES, default 20 KB)."""
    raw = os.environ.get("ULTRON_RLM_OUTPUT_BYTES", "")
    try:
        value = int(raw)
    except ValueError:
        return 20_000
    return value if value > 0 else 20_000


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
        return (
            head
            + f"\n[... {self._total - kept} bytes truncated{where} ...]\n"
            + tail.decode("utf-8", errors="ignore")
        )


def bind_spilled_output(stdout: _MiddleTextIO, namespace: dict[str, Any]) -> None:
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
        namespace["last_output"] = ContextHandle(text, digest, "last_output", len(raw))


def _live_output_enabled() -> bool:
    value = os.environ.get("ULTRON_RLM_STREAM_OUTPUT", "").strip().lower()
    return value not in ("", "0", "off", "false", "no")


class _LiveOutput:
    """A running cell's printed output, sent to the host as it is written (`cell.output` host requests that nothing
    waits on), at most every _LIVE_INTERVAL seconds from a timer thread, so output before a long blocking call is
    sent too. The cell's own result still carries its whole captured output."""

    def __init__(self, request_id: str, send_frame: Callable[[dict[str, Any]], None]) -> None:
        self._request_id = request_id
        self._send_frame = send_frame
        self._lock = threading.Lock()
        self._pending: list[str] = []
        self._size = 0
        self._dropped = 0
        self._timer: threading.Timer | None = None
        self._closed = False
        self._sent = 0

    @staticmethod
    def open(
        request_id: str,
        *,
        ready: bool,
        send_frame: Callable[[dict[str, Any]], None],
    ) -> "_LiveOutput | None":
        return _LiveOutput(request_id, send_frame) if _live_output_enabled() and ready else None

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
        with contextlib.suppress(BaseException):
            self._send_frame({
                "event": "host_request",
                "id": frame_id,
                "cell": self._request_id,
                "type": "cell.output",
                "payload": payload,
            })

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

    def __init__(self, cell: str, target: io.TextIOBase, cell_var: contextvars.ContextVar[str | None]) -> None:
        super().__init__()
        self._cell = cell
        self._target = target
        self._cell_var = cell_var
        self.dropped = 0

    def writable(self) -> bool:
        return True

    def write(self, text: str) -> int:
        owner = self._cell_var.get()
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
