"""Image input validation, serialization, and `view_image` bridge helper for `runtime`."""
from __future__ import annotations

import contextlib
import importlib
import importlib.util
import io
import os
import tempfile
from pathlib import Path
from typing import Any

from worktree_api import worktree_path


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
    if importlib.util.find_spec("PIL.Image") is None:
        return None
    pil_image = importlib.import_module("PIL.Image")
    if not isinstance(source, pil_image.Image):
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


async def view_image_with_bridge(
    bridge: Any,
    path_or_bytes: Any,
    *,
    detail: str | None = None,
) -> str:
    """Validate and send an image to the host via `bridge`."""
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
        reply = await bridge.request("rlm.view_image", payload)
    finally:
        _cleanup_temp_image(temporary)
    return _format_view_image_reply(reply)
