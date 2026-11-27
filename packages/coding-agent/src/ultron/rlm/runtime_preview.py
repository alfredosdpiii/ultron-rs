"""Bounded value, dataframe, reference, and error preview rendering for the RLM runtime."""
from __future__ import annotations

import contextlib
import hashlib
import io
import signal
import sys
import threading
import traceback
from typing import Any

_PREVIEW_BYTES = 8192
_TRUNCATION_MARKER = "\n... [truncated]"
_MARKER_BYTES = len(_TRUNCATION_MARKER.encode("utf-8"))
_PREVIEW_SECONDS = 2.0

_REFERENCE_CHARS = 2_000
_REFERENCE_ITEMS = 40
_REFERENCE_HEAD = 600
_REFERENCE_TAIL = 300
_REFERENCE_HINT = (
    "The whole value is kept in the kernel as `_`: "
    "slice it, search it, or use preview(_) instead of showing it all."
)

_deadline_active = False
# Once a preview deadline fires, no further user __repr__/__str__ runs for that preview.
_deadline_expired = False

_SIMPLE_DELIMS: dict[type, tuple[str, str]] = {
    dict: ("{", "}"),
    list: ("[", "]"),
    tuple: ("(", ")"),
}
_CYCLE_PLACEHOLDERS: dict[type, str] = {
    dict: "{...}",
    list: "[...]",
}


def _type_name(kind: type) -> str:
    return f"{kind.__module__}.{kind.__qualname__}"


class _BoundedTextIO(io.TextIOBase):
    """Keep a UTF-8 prefix, never retaining more than the preview budget."""

    def __init__(self, limit: int = _PREVIEW_BYTES) -> None:
        super().__init__()
        self._limit = limit
        self._buffer = bytearray()
        self.truncated = False

    def writable(self) -> bool:
        return True

    def truncate_preview(self) -> None:
        self.truncated = True
        del self._buffer[self._limit - _MARKER_BYTES :]

    def write(self, text: str) -> int:
        if self.closed:
            raise ValueError("I/O operation on closed file")
        if not isinstance(text, str):
            raise TypeError("write() argument must be str")
        if not self.truncated:
            remaining = self._limit - len(self._buffer)
            # Slice before encoding so a huge write needs bounded temporary space.
            encoded = text[: remaining + 1].encode("utf-8", errors="backslashreplace")
            self._buffer.extend(encoded[:remaining])
            if len(encoded) > remaining:
                self.truncate_preview()
        return len(text)

    def getvalue(self) -> str:
        # Drop a partial final code point rather than add a replacement glyph.
        suffix = _TRUNCATION_MARKER if self.truncated else ""
        return self._buffer.decode("utf-8", errors="ignore") + suffix


def _text_preview(text: str) -> str:
    stream = _BoundedTextIO()
    stream.write(text)
    return stream.getvalue()


class _PreviewTimeout(BaseException):
    pass


@contextlib.contextmanager
def _preview_deadline(seconds: float = _PREVIEW_SECONDS):
    """Interrupt user __repr__/__str__ code that runs during preview rendering."""
    global _deadline_active, _deadline_expired
    usable = (
        not _deadline_active
        and hasattr(signal, "setitimer")
        and threading.current_thread() is threading.main_thread()
    )
    if not usable:
        yield
        return

    def on_alarm(_signum: int, _frame: Any) -> None:
        global _deadline_expired
        _deadline_expired = True
        raise _PreviewTimeout()

    previous = signal.signal(signal.SIGALRM, on_alarm)
    _deadline_active = True
    _deadline_expired = False
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)
        _deadline_active = False
        _deadline_expired = False


def _safe_repr(item: Any) -> str:
    if _deadline_expired:
        raise _PreviewTimeout()
    try:
        text = repr(item)
        if type(text) is not str:
            raise TypeError("__repr__ returned non-string")
        return text
    except _PreviewTimeout:
        raise
    except BaseException as error:
        return f"<{type(item).__qualname__} object; repr raised {type(error).__name__}>"


def _render_text_or_bytes(
    item: str | bytes | bytearray, stream: _BoundedTextIO, limit: int
) -> None:
    prefix = item[:limit]
    stream.write(repr(prefix))
    if len(item) > len(prefix):
        stream.truncate_preview()


def _render_int(item: int, stream: _BoundedTextIO, limit: int) -> None:
    # Avoid both enormous decimal strings and Python's conversion limit.
    digits = item.bit_length() * 30103 // 100000 + 1
    int_limit = getattr(sys, "get_int_max_str_digits", lambda: 0)()
    if digits + (item < 0) > limit or (int_limit and digits > int_limit):
        stream.write(f"<int with {item.bit_length()} bits>")
        stream.truncate_preview()
        return
    stream.write(repr(item))


def _render_scalar(item: Any, stream: _BoundedTextIO, limit: int) -> bool:
    kind = type(item)
    if kind in (str, bytes, bytearray):
        _render_text_or_bytes(item, stream, limit)
        return True
    if kind is int:
        _render_int(item, stream, limit)
        return True
    containers = (list, tuple, dict, set, frozenset, range, slice)
    is_exception = isinstance(item, BaseException) and kind.__repr__ is BaseException.__repr__
    if kind not in containers and not is_exception:
        stream.write(_safe_repr(item))
        return True
    return False


def _seq_container_spec(
    item: Any, kind: type, stream: _BoundedTextIO
) -> tuple[str, str, Any] | None:
    if kind in _SIMPLE_DELIMS:
        opening, closing = _SIMPLE_DELIMS[kind]
        return opening, closing, iter(item)
    if kind in (set, frozenset):
        if not item:
            stream.write("set()" if kind is set else "frozenset()")
            return None
        opening = "{" if kind is set else "frozenset({"
        closing = "}" if kind is set else "})"
        return opening, closing, iter(item)
    return None


def _range_slice_spec(
    item: Any, kind: type, stream: _BoundedTextIO
) -> tuple[str, str, Any]:
    if kind is range:
        parts = (
            (item.start, item.stop)
            if item.step == 1
            else (item.start, item.stop, item.step)
        )
        return "range(", ")", iter(parts)
    if kind is slice:
        return "slice(", ")", iter((item.start, item.stop, item.step))
    stream.write(kind.__name__)
    return "(", ")", iter(item.args)


def _container_spec(
    item: Any, kind: type, stream: _BoundedTextIO
) -> tuple[str, str, Any] | None:
    if kind in (dict, list, tuple, set, frozenset):
        return _seq_container_spec(item, kind, stream)
    return _range_slice_spec(item, kind, stream)


def _render_container_items(
    item: Any,
    kind: type,
    spec: tuple[str, str, Any],
    stream: _BoundedTextIO,
    render_fn: Any,
    depth: int,
) -> None:
    opening, closing, values = spec
    stream.write(opening)
    count = 0
    while not stream.truncated:
        try:
            child = next(values)
        except StopIteration:
            break
        if count:
            stream.write(", ")
        render_fn(child, depth + 1)
        if kind is dict and not stream.truncated:
            stream.write(": ")
            render_fn(item[child], depth + 1)
        count += 1
    if kind is tuple and count == 1:
        stream.write(",")
    stream.write(closing)


def _render_preview(
    value: Any, limit: int = _PREVIEW_BYTES, max_depth: int = 20
) -> tuple[str, bool]:
    """Render builtins incrementally without copying or traversing whole values."""
    stream = _BoundedTextIO(limit)
    active: set[int] = set()

    def render(item: Any, depth: int = 0) -> None:
        if stream.truncated or _render_scalar(item, stream, limit):
            return
        kind = type(item)
        if id(item) in active:
            stream.write(_CYCLE_PLACEHOLDERS.get(kind, "(...)"))
            return
        if depth >= max_depth:
            stream.truncate_preview()
            return
        active.add(id(item))
        try:
            spec = _container_spec(item, kind, stream)
            if spec is not None:
                _render_container_items(item, kind, spec, stream, render, depth)
        finally:
            active.remove(id(item))

    try:
        render(value)
    except _PreviewTimeout:
        stream.write(" <preview timed out>")
        stream.truncated = True
    except RecursionError:
        stream.write(" <preview recursion limit>")
        stream.truncated = True
    return stream.getvalue(), stream.truncated


def _result_preview(
    value: Any, limit: int = _PREVIEW_BYTES, max_depth: int = 20
) -> str:
    return _render_preview(value, limit, max_depth)[0]


def preview(
    value: Any, depth: int = 20, max_bytes: int = _PREVIEW_BYTES
) -> dict[str, Any]:
    """Bounded, model-visible description of a value. The value itself is untouched."""
    if type(depth) is not int or not 1 <= depth <= 100:
        raise ValueError("preview depth must be an integer between 1 and 100")
    if type(max_bytes) is not int or not 64 <= max_bytes <= 1024 * 1024:
        raise ValueError("preview max_bytes must be an integer between 64 and 1048576")
    kind = type(value)
    sized = (str, bytes, bytearray, list, tuple, dict, set, frozenset, range)
    with _preview_deadline():
        text, truncated = _render_preview(value, max_bytes, depth)
    return {
        "type": _type_name(kind),
        "length": len(value) if kind in sized else None,
        "preview": text,
        "truncated": truncated,
        "max_bytes": max_bytes,
    }


def _digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()[:12]


def _items_preview(items: list[Any]) -> str:
    text, _truncated = _render_preview(items, _REFERENCE_HEAD * 2, 6)
    return text


def _dataframe_preview(value: Any, kind: type, name: str) -> str:
    shape = "x".join(str(int(dim)) for dim in value.shape)
    lines = [f"<{name} {shape}, {_type_name(kind)}>"]
    if name == "DataFrame":
        cols = ", ".join(f"{col} ({dtype})" for col, dtype in value.dtypes.items())
        lines.append("columns: " + _text_preview(cols))
    lines.append("head:\n" + _text_preview(str(value.head(5))))
    if int(value.shape[0]) > 8:
        lines.append("tail:\n" + _text_preview(str(value.tail(3))))
    lines.append(
        "It stays in the kernel as `_`: filter, aggregate or `.to_string()` "
        "a slice instead of showing it all."
    )
    return "\n".join(lines)


def _ndarray_preview(value: Any, kind: type) -> str | None:
    if int(value.size) <= _REFERENCE_ITEMS:
        return None
    shape = "x".join(str(int(dim)) for dim in value.shape)
    flat = value.ravel()
    return "\n".join([
        f"<ndarray {shape} dtype={value.dtype}, {_type_name(kind)}>",
        "head: " + _text_preview(str(flat[: _REFERENCE_ITEMS // 2])),
        "tail: " + _text_preview(str(flat[-(_REFERENCE_ITEMS // 4) :])),
        _REFERENCE_HINT,
    ])


def _frame_preview(value: Any) -> str | None:
    """A pandas DataFrame/Series or numpy array, described without importing those packages."""
    kind = type(value)
    name = kind.__name__
    if name in ("DataFrame", "Series") and hasattr(value, "shape") and hasattr(value, "head"):
        return _dataframe_preview(value, kind, name)
    if name == "ndarray" and hasattr(value, "shape") and hasattr(value, "dtype"):
        return _ndarray_preview(value, kind)
    return None


def _str_reference(value: str, _kind: type) -> str | None:
    if len(value) <= _REFERENCE_CHARS:
        return None
    digest = _digest(value.encode("utf-8", "surrogatepass"))
    lines = value.count(chr(10)) + 1
    return "\n".join([
        f"<str: {len(value):,} chars, {lines:,} lines, sha256 {digest}>",
        f"head: {value[:_REFERENCE_HEAD]!r}",
        f"tail: {value[-_REFERENCE_TAIL:]!r}",
        _REFERENCE_HINT,
    ])


def _bytes_reference(value: bytes | bytearray, kind: type) -> str | None:
    if len(value) <= _REFERENCE_CHARS:
        return None
    digest = _digest(bytes(value))
    return "\n".join([
        f"<{kind.__name__}: {len(value):,} bytes, sha256 {digest}>",
        f"head: {bytes(value[: _REFERENCE_HEAD // 4])!r}",
        f"tail: {bytes(value[-(_REFERENCE_TAIL // 4) :])!r}",
        _REFERENCE_HINT,
    ])


def _seq_reference(value: list[Any] | tuple[Any, ...], kind: type) -> str | None:
    if len(value) <= _REFERENCE_ITEMS:
        return None
    kinds = sorted({type(item).__name__ for item in value[:200]})
    return "\n".join([
        f"<{kind.__name__}: {len(value):,} items; item types {', '.join(kinds)}>",
        f"head: {_items_preview(list(value[:10]))}",
        f"tail: {_items_preview(list(value[-5:]))}",
        _REFERENCE_HINT,
    ])


def _dict_reference(value: dict[Any, Any], _kind: type) -> str | None:
    if len(value) <= _REFERENCE_ITEMS:
        return None
    keys = list(value)
    return "\n".join([
        f"<dict: {len(value):,} keys>",
        f"first keys: {_items_preview(keys[:10])}",
        f"last keys: {_items_preview(keys[-5:])}",
        f"first item: {_items_preview([keys[0], value[keys[0]]])}",
        _REFERENCE_HINT,
    ])


def _set_reference(value: set[Any] | frozenset[Any], kind: type) -> str | None:
    if len(value) <= _REFERENCE_ITEMS:
        return None
    sample: list[Any] = []
    for item in value:
        sample.append(item)
        if len(sample) == 10:
            break
    return "\n".join([
        f"<{kind.__name__}: {len(value):,} items>",
        f"sample: {_items_preview(sample)}",
        _REFERENCE_HINT,
    ])


_REFERENCE_RENDERERS: dict[type, Any] = {
    str: _str_reference,
    bytes: _bytes_reference,
    bytearray: _bytes_reference,
    list: _seq_reference,
    tuple: _seq_reference,
    dict: _dict_reference,
    set: _set_reference,
    frozenset: _set_reference,
}


def _reference_preview(value: Any) -> str | None:
    """Return a bounded by-reference summary of a large value, or None if small."""
    kind = type(value)
    renderer = _REFERENCE_RENDERERS.get(kind)
    if renderer is not None:
        return renderer(value, kind)
    with contextlib.suppress(BaseException):
        return _frame_preview(value)
    return None


def _cell_result_text(value: Any) -> str:
    """The text shown for a cell's last expression: whole when small, by reference when large."""
    reference = _reference_preview(value)
    if reference is not None:
        return reference
    text, truncated = _render_preview(value)
    if truncated:
        return f"{text}\n{_REFERENCE_HINT}"
    return text


def _exception_message(error: BaseException) -> str:
    if _deadline_expired:
        return "<exception str() timed out>"
    try:
        if type(error).__str__ is BaseException.__str__:
            if not error.args:
                return ""
            if len(error.args) != 1:
                return _result_preview(error.args)
            value = error.args[0]
            if type(value) is str:
                return _text_preview(value)
            previewed_types = (
                int, bytes, bytearray, list, tuple, dict, set, frozenset, range, slice
            )
            if type(value) in previewed_types:
                return _result_preview(value)
            return _text_preview(str(value))
        if type(error) is KeyError and len(error.args) == 1:
            return _result_preview(error.args[0])
        return _text_preview(str(error))
    except BaseException:
        return "<exception str() failed>"


def _render_chained_exception(
    current: BaseException,
    stream: _BoundedTextIO,
    render_fn: Any,
    depth: int,
) -> None:
    cause = current.__cause__
    context = current.__context__
    if cause is not None:
        render_fn(cause, depth + 1)
        stream.write(
            "\nThe above exception was the direct cause of the following exception:\n\n"
        )
    elif context is not None and not current.__suppress_context__:
        render_fn(context, depth + 1)
        stream.write(
            "\nDuring handling of the above exception, another exception occurred:\n\n"
        )


def _render_traceback(
    tb: Any,
    stream: _BoundedTextIO,
    skip_file: str | None,
    frame_source_line: Any,
) -> None:
    stream.write("Traceback (most recent call last):\n")
    for frame, lineno in traceback.walk_tb(tb):
        code = frame.f_code
        if code.co_name == "execute_cell" and code.co_filename == skip_file:
            continue  # the kernel's own frame around every cell
        stream.write(f'  File "{code.co_filename}", line {lineno}, in {code.co_name}\n')
        line = frame_source_line(code, lineno) if frame_source_line else ""
        if line:
            stream.write(f"    {line[:300]}\n")
        if stream.truncated:
            return


def render_error_preview(
    error: BaseException,
    *,
    skip_file: str | None = None,
    frame_source_line: Any = None,
) -> dict[str, Any]:
    stream = _BoundedTextIO()
    seen: set[int] = set()

    def render(current: BaseException, depth: int = 0) -> None:
        if stream.truncated or id(current) in seen:
            return
        if depth >= 20:
            stream.truncate_preview()
            return
        seen.add(id(current))
        _render_chained_exception(current, stream, render, depth)
        if stream.truncated:
            return
        if current.__traceback__ is not None:
            _render_traceback(
                current.__traceback__, stream, skip_file, frame_source_line
            )
            if stream.truncated:
                return
        stream.write(f"{type(current).__name__}: {_exception_message(current)}\n")

    render(error)
    return {
        "ename": _text_preview(type(error).__name__),
        "evalue": _exception_message(error),
        "traceback": stream.getvalue().splitlines(),
    }
