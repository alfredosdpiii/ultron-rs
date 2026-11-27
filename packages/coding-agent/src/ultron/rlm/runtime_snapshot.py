"""Snapshot encoding, decoding, validation, and restore helpers for the RLM runtime."""
from __future__ import annotations

import ast
import base64
import datetime
from decimal import Decimal, InvalidOperation
import hashlib
import json
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import re
from typing import Any
import uuid

import agent_class_api
from runtime_preview import _text_preview, _type_name

_SNAPSHOT_FORMAT = "ultron-rlm-snapshot"
_SNAPSHOT_VERSION = 2
_SIGNATURE_FORMAT = "ultron-rlm-snapshot-signature"
_SCALARS = frozenset((type(None), bool, int, str, float))
_PATH_TYPE = type(Path())
_SNAPSHOT_VAR_MAX_BYTES_DEFAULT = 2 * 1024 * 1024
_SNAPSHOT_MAX_BYTES_DEFAULT = 16 * 1024 * 1024
_DEFINING = (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef, ast.Import, ast.ImportFrom)


def _snapshot_cap(env_name: str, default: int) -> int:
    raw = os.environ.get(env_name, "").strip()
    if not raw:
        return default
    try:
        value = int(raw)
    except ValueError:
        return default
    return value if value > 0 else 0


class _NonRestorable(Exception):
    pass


class SnapshotIntegrityError(Exception):
    pass


def _track_mutable(value: Any, mutable: set[int]) -> None:
    if id(value) in mutable:
        raise _NonRestorable("shared or cyclic mutable reference")
    mutable.add(id(value))


def _encode_regex(value: re.Pattern[Any]) -> dict[str, Any]:
    if type(value.pattern) is not str:
        raise _NonRestorable(f"unsupported type {_type_name(type(value))}")
    return {"$regex": [value.pattern, value.flags]}


_STDLIB_ENCODERS: dict[type, Any] = {
    _PATH_TYPE: lambda value: {"$path": str(value)},
    PurePosixPath: lambda value: {"$pure_posix_path": str(value)},
    PureWindowsPath: lambda value: {"$pure_windows_path": str(value)},
    datetime.datetime: lambda value: {"$datetime": value.isoformat()},
    datetime.date: lambda value: {"$date": value.isoformat()},
    datetime.timedelta: lambda value: {
        "$timedelta": [value.days, value.seconds, value.microseconds]
    },
    Decimal: lambda value: {"$decimal": str(value)},
    uuid.UUID: lambda value: {"$uuid": str(value)},
    re.Pattern: _encode_regex,
}


def _is_plain_scalars(items: Any) -> bool:
    return len(items) > 16 and set(map(type, items)) <= _SCALARS


def _encode_bytes(
    value: bytes | bytearray, kind: type, mutable: set[int]
) -> dict[str, str]:
    if kind is bytearray:
        _track_mutable(value, mutable)
    encoded = base64.b64encode(bytes(value)).decode("ascii")
    return {f"${kind.__name__}": encoded}


def _encode_sequence(value: Any, kind: type, mutable: set[int]) -> Any:
    if kind in (list, set):
        _track_mutable(value, mutable)
    if _is_plain_scalars(value):
        items = list(value)
    else:
        items = [
            item if type(item) in _SCALARS else _encode_value(item, mutable)
            for item in value
        ]
    if kind is list:
        return items
    return {f"${kind.__name__}": items}


def _encode_dict(value: dict[Any, Any], mutable: set[int]) -> dict[str, Any]:
    _track_mutable(value, mutable)
    if _is_plain_scalars(value) and _is_plain_scalars(value.values()):
        return {"$dict": list(map(list, value.items()))}
    pairs: list[list[Any]] = []
    for key, item in value.items():
        enc_key = key if type(key) in _SCALARS else _encode_value(key, mutable)
        enc_item = item if type(item) in _SCALARS else _encode_value(item, mutable)
        pairs.append([enc_key, enc_item])
    return {"$dict": pairs}


def _is_list_subclass(value: Any, kind: type) -> bool:
    # A list subclass that only adds metadata (rlm.map's MapResults) is kept as plain items.
    return (
        kind is not list
        and isinstance(value, list)
        and bool(getattr(kind, "_snapshot_as_list", False))
    )


def _encode_value(value: Any, mutable: set[int]) -> Any:
    """Encode exact builtin and safe immutable stdlib data types."""
    kind = type(value)
    if kind in _SCALARS:
        return value
    if _is_list_subclass(value, kind):
        kind = list
    encoder = _STDLIB_ENCODERS.get(kind)
    if encoder is not None:
        return encoder(value)
    if kind in (bytes, bytearray):
        return _encode_bytes(value, kind, mutable)
    if kind in (list, tuple, set, frozenset):
        return _encode_sequence(value, kind, mutable)
    if kind is dict:
        return _encode_dict(value, mutable)
    raise _NonRestorable(f"unsupported type {_type_name(kind)}")


def _decode_bytes(body: str) -> bytes:
    return base64.b64decode(body.encode("ascii"), validate=True)


def _decode_bytearray(body: str) -> bytearray:
    return bytearray(_decode_bytes(body))


def _decode_datetime(body: str) -> datetime.datetime:
    try:
        return datetime.datetime.fromisoformat(body)
    except ValueError as error:
        raise SnapshotIntegrityError(
            f"snapshot contains a malformed datetime: {error}"
        ) from error


def _decode_date(body: str) -> datetime.date:
    try:
        return datetime.date.fromisoformat(body)
    except ValueError as error:
        raise SnapshotIntegrityError(
            f"snapshot contains a malformed date: {error}"
        ) from error


def _decode_decimal(body: str) -> Decimal:
    try:
        return Decimal(body)
    except InvalidOperation as error:
        raise SnapshotIntegrityError(
            f"snapshot contains a malformed decimal: {error}"
        ) from error


def _decode_uuid(body: str) -> uuid.UUID:
    try:
        return uuid.UUID(body)
    except ValueError as error:
        raise SnapshotIntegrityError(
            f"snapshot contains a malformed uuid: {error}"
        ) from error


def _decode_timedelta(body: list[Any]) -> datetime.timedelta:
    if len(body) != 3:
        raise SnapshotIntegrityError("snapshot contains a malformed timedelta")
    for part in body:
        if type(part) is not int:
            raise SnapshotIntegrityError("snapshot contains a malformed timedelta")
    return datetime.timedelta(days=body[0], seconds=body[1], microseconds=body[2])


def _decode_regex(body: list[Any]) -> re.Pattern[str]:
    if len(body) != 2 or type(body[0]) is not str or type(body[1]) is not int:
        raise SnapshotIntegrityError("snapshot contains a malformed regex")
    try:
        return re.compile(body[0], body[1])
    except re.error as error:
        raise SnapshotIntegrityError(
            f"snapshot contains a malformed regex: {error}"
        ) from error


def _decode_dict(body: list[Any]) -> dict[Any, Any]:
    result: dict[Any, Any] = {}
    for pair in body:
        if type(pair) is not list or len(pair) != 2:
            raise SnapshotIntegrityError("snapshot contains a malformed dict")
        result[_decode_value(pair[0])] = _decode_value(pair[1])
    return result


_STR_TAG_DECODERS: dict[str, Any] = {
    "$bytes": _decode_bytes,
    "$bytearray": _decode_bytearray,
    "$path": Path,
    "$pure_posix_path": PurePosixPath,
    "$pure_windows_path": PureWindowsPath,
    "$datetime": _decode_datetime,
    "$date": _decode_date,
    "$decimal": _decode_decimal,
    "$uuid": _decode_uuid,
}

_LIST_TAG_DECODERS: dict[str, Any] = {
    "$timedelta": _decode_timedelta,
    "$regex": _decode_regex,
    "$tuple": lambda body: tuple(_decode_value(item) for item in body),
    "$set": lambda body: {_decode_value(item) for item in body},
    "$frozenset": lambda body: frozenset(_decode_value(item) for item in body),
    "$dict": _decode_dict,
}


def _decode_tagged(tag: Any, body: Any) -> Any:
    if tag in _STR_TAG_DECODERS:
        if type(body) is not str:
            raise SnapshotIntegrityError("snapshot contains a malformed value")
        return _STR_TAG_DECODERS[tag](body)
    if tag in _LIST_TAG_DECODERS:
        if type(body) is not list:
            raise SnapshotIntegrityError("snapshot contains a malformed value")
        return _LIST_TAG_DECODERS[tag](body)
    raise SnapshotIntegrityError(
        f"snapshot contains an unknown tag: {_text_preview(str(tag))}"
    )


def _decode_value(value: Any) -> Any:
    kind = type(value)
    if value is None or kind in (bool, int, str, float):
        return value
    if kind is list:
        return [_decode_value(item) for item in value]
    if kind is not dict or len(value) != 1:
        raise SnapshotIntegrityError("snapshot contains a malformed value")
    tag, body = next(iter(value.items()))
    return _decode_tagged(tag, body)


def _checked_definitions(value: Any) -> list[tuple[str, str]]:
    """A snapshot's definitions: each must be one def, class or import statement."""
    if type(value) is not list:
        raise SnapshotIntegrityError(
            "snapshot integrity check failed: malformed definitions"
        )
    checked: list[tuple[str, str]] = []
    for item in value:
        valid_pair = (
            type(item) is list
            and len(item) == 2
            and type(item[0]) is str
            and type(item[1]) is str
        )
        if not valid_pair:
            raise SnapshotIntegrityError(
                "snapshot integrity check failed: malformed definitions"
            )
        try:
            body = ast.parse(item[1], filename="<rlm-cell>", mode="exec").body
        except SyntaxError:
            body = []
        if len(body) != 1 or not isinstance(body[0], _DEFINING):
            raise SnapshotIntegrityError(
                "snapshot integrity check failed: a definition is not one def, class or import"
            )
        checked.append((item[0], item[1]))
    return checked


def _try_encode_binding(
    name: str,
    value: Any,
    definitions: dict[str, tuple[str, Any]],
    defined: set[str],
    reasons: dict[str, str],
) -> tuple[str, set[int]] | None:
    mutable: set[int] = set()
    encoded_pair: tuple[str, set[int]] | None = None
    try:
        candidate = json.dumps(
            _encode_value(value, mutable), separators=(",", ":"), allow_nan=True
        )
        encoded_pair = (candidate, mutable)
    except _NonRestorable as error:
        # Agent classes come back from `state` (agent_class_api.rehydrate), not from their statement.
        is_def = name in definitions and definitions[name][1] is value
        if is_def and getattr(value, "__agent__", None) is None:
            defined.add(name)
        else:
            reasons[name] = str(error)
    except Exception as error:
        reasons[name] = f"not encodable: {type(error).__name__}"
    return encoded_pair


def _check_snapshot_caps(
    name: str,
    size: int,
    total_bytes: int,
    var_cap: int,
    total_cap: int,
    reasons: dict[str, str],
) -> bool:
    if var_cap > 0 and size > var_cap:
        reasons[name] = f"exceeds per-variable snapshot cap ({size} > {var_cap} bytes)"
        return False
    if total_cap > 0 and total_bytes + size > total_cap:
        reasons[name] = (
            f"exceeds aggregate snapshot cap ({total_bytes + size} > {total_cap} bytes)"
        )
        return False
    return True


def _evict_aliased_owners(
    name: str,
    mutable: set[int],
    owners: dict[int, str],
    encoded: dict[str, str],
    sizes: dict[str, int],
    reasons: dict[str, str],
) -> int | None:
    shared = sorted({owners[item] for item in mutable if item in owners})
    if not shared:
        return None
    reasons[name] = "aliases mutable data of " + ", ".join(shared)
    reclaimed = 0
    for other in shared:
        if other in encoded:
            encoded.pop(other, None)
            reclaimed += sizes.pop(other, 0)
        reasons.setdefault(other, "aliases mutable data of " + name)
    return reclaimed


def _is_skipped_binding(
    name: str, value: Any, bindings: dict[str, Any]
) -> bool:
    return name.startswith("__") or (name in bindings and value is bindings[name])


def _kept_definitions(
    definitions: dict[str, tuple[str, Any]], defined: set[str]
) -> list[tuple[str, str]]:
    kept: list[tuple[str, str]] = []
    for name, (source, _) in definitions.items():
        if name in defined:
            kept.append((name, source))
    return kept


def _persistable_namespace(
    namespace: dict[str, Any],
    bindings: dict[str, Any],
    definitions: dict[str, tuple[str, Any]],
) -> tuple[dict[str, str], dict[str, str], list[tuple[str, str]]]:
    """Encode restorable names, enforce byte caps (encoding `state` first), and record skip reasons."""
    encoded: dict[str, str] = {}
    sizes: dict[str, int] = {}
    reasons: dict[str, str] = {}
    owners: dict[int, str] = {}
    defined: set[str] = set()
    var_cap = _snapshot_cap("ULTRON_SNAPSHOT_VAR_MAX_BYTES", _SNAPSHOT_VAR_MAX_BYTES_DEFAULT)
    total_cap = _snapshot_cap("ULTRON_SNAPSHOT_MAX_BYTES", _SNAPSHOT_MAX_BYTES_DEFAULT)
    total_bytes = 0
    items = sorted(namespace.items(), key=lambda pair: 0 if pair[0] == "state" else 1)
    for name, value in items:
        if _is_skipped_binding(name, value, bindings):
            continue
        attempt = _try_encode_binding(name, value, definitions, defined, reasons)
        if attempt is None:
            continue
        candidate, mutable = attempt
        size = len(candidate.encode("utf-8"))
        if not _check_snapshot_caps(name, size, total_bytes, var_cap, total_cap, reasons):
            continue
        reclaimed = _evict_aliased_owners(name, mutable, owners, encoded, sizes, reasons)
        if reclaimed is not None:
            total_bytes -= reclaimed
            continue
        for item in mutable:
            owners[item] = name
        encoded[name] = candidate
        sizes[name] = size
        total_bytes += size
    return encoded, reasons, _kept_definitions(definitions, defined)


def save_snapshot_file(
    path: str,
    namespace: dict[str, Any],
    bindings: dict[str, Any],
    definitions: dict[str, tuple[str, Any]],
) -> dict[str, Any]:
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    agent_class_api.flush()
    encoded, reasons, kept_defs = _persistable_namespace(namespace, bindings, definitions)
    skipped = sorted(reasons)
    rest = json.dumps(
        {"defs": kept_defs, "skipped": skipped, "reasons": reasons},
        separators=(",", ":"),
        allow_nan=True,
    )
    entries = ",".join(json.dumps(name) + ":" + text for name, text in encoded.items())
    body = ('{"names":{' + entries + "}," + rest[1:]).encode()
    digest = hashlib.sha256(body).hexdigest()
    header = json.dumps(
        {"format": _SNAPSHOT_FORMAT, "version": _SNAPSHOT_VERSION, "sha256": digest, "bytes": len(body)},
        separators=(",", ":"),
    ).encode("utf-8")
    content = header + b"\n" + body
    temporary = target.with_name(f".{target.name}.{os.getpid()}.tmp")
    temporary.write_bytes(content)
    os.replace(temporary, target)
    return {
        "saved": sorted([*encoded, *(name for name, _ in kept_defs)]),
        "skipped": skipped,
        "reasons": reasons,
        "serializer": "json",
        "sha256": digest,
        # The host signs these exact bytes; it refuses to sign a file that changed after this write.
        "content_sha256": hashlib.sha256(content).hexdigest(),
    }


def _parse_json_line(
    line: bytes, has_separator: bytes, error_message: str
) -> dict[str, Any]:
    if not has_separator:
        raise SnapshotIntegrityError(error_message)
    try:
        parsed = json.loads(line)
    except ValueError as error:
        raise SnapshotIntegrityError(error_message) from error
    if type(parsed) is not dict:
        raise SnapshotIntegrityError(error_message)
    return parsed


def _verify_snapshot_signature(raw: bytes, verified_sha256: Any) -> bytes:
    signature_line, separator, content = raw.partition(b"\n")
    missing_sig = (
        "snapshot integrity check failed: missing host signature "
        "(unsigned snapshots are refused)"
    )
    signature = _parse_json_line(signature_line, separator, missing_sig)
    if signature.get("format") != _SIGNATURE_FORMAT:
        raise SnapshotIntegrityError(missing_sig)
    if type(verified_sha256) is not str or hashlib.sha256(content).hexdigest() != verified_sha256:
        raise SnapshotIntegrityError(
            "snapshot integrity check failed: content differs from the host-verified snapshot"
        )
    return content


def _validate_snapshot_header(header: dict[str, Any], body: bytes) -> None:
    if header.get("format") != _SNAPSHOT_FORMAT or header.get("version") != _SNAPSHOT_VERSION:
        raise SnapshotIntegrityError(
            "snapshot integrity check failed: missing or unsupported header"
        )
    if type(header.get("sha256")) is not str or type(header.get("bytes")) is not int:
        raise SnapshotIntegrityError(
            "snapshot integrity check failed: missing or unsupported header"
        )
    if header["bytes"] != len(body) or hashlib.sha256(body).hexdigest() != header["sha256"]:
        raise SnapshotIntegrityError("snapshot integrity check failed: digest mismatch")


def _parse_snapshot_payload(content: bytes) -> Any:
    header_line, separator, body = content.partition(b"\n")
    header = _parse_json_line(
        header_line,
        separator,
        "snapshot integrity check failed: missing or unsupported header",
    )
    _validate_snapshot_header(header, body)
    return json.loads(body.decode("utf-8"))


def _validate_snapshot_body(
    data: Any,
) -> tuple[dict[str, Any], list[Any], dict[str, Any]]:
    if type(data) is not dict:
        raise SnapshotIntegrityError("snapshot integrity check failed: malformed body")
    names = data.get("names")
    skipped = data.get("skipped")
    reasons = data.get("reasons")
    if type(names) is not dict or type(skipped) is not list or type(reasons) is not dict:
        raise SnapshotIntegrityError("snapshot integrity check failed: malformed body")
    for item in [*names, *skipped, *reasons.values()]:
        if type(item) is not str:
            raise SnapshotIntegrityError("snapshot integrity check failed: malformed body")
    return names, skipped, reasons


def _apply_snapshot_definitions(
    definitions: list[tuple[str, str]],
    namespace: dict[str, Any],
    defs_store: dict[str, tuple[str, Any]],
    remember_fn: Any,
    skipped: list[Any],
    reasons: dict[str, Any],
) -> list[str]:
    redefined: list[str] = []
    for name, source in definitions:
        try:
            code = compile(source, "<rlm-cell>", "exec")
            remember_fn(code, source)
            exec(code, namespace, namespace)
            defs_store[name] = (source, namespace[name])
            redefined.append(name)
        except Exception as error:
            skipped.append(name)
            reasons[name] = f"definition failed on restore: {type(error).__name__}: {error}"
    return redefined


def restore_snapshot_file(
    path: str,
    verified_sha256: Any,
    namespace: dict[str, Any],
    defs_store: dict[str, tuple[str, Any]],
    remember_fn: Any,
) -> dict[str, Any]:
    target = Path(path)
    if not target.exists():
        return {"restored": [], "missing": True, "skipped": [], "reasons": {}}
    content = _verify_snapshot_signature(target.read_bytes(), verified_sha256)
    data = _parse_snapshot_payload(content)
    names, skipped, reasons = _validate_snapshot_body(data)
    # Decode everything before touching the namespace, so a bad snapshot changes nothing.
    decoded = {name: _decode_value(value) for name, value in names.items()}
    definitions = _checked_definitions(data.get("defs", []))
    before = dict(namespace)
    namespace.update(decoded)
    try:
        redefined = _apply_snapshot_definitions(
            definitions, namespace, defs_store, remember_fn, skipped, reasons
        )
        agent_class_api.rehydrate()
    except BaseException:
        # Defining stored agent classes failed: restore previous namespace.
        namespace.clear()
        namespace.update(before)
        raise
    return {
        "restored": sorted([*decoded, *redefined]),
        "missing": False,
        "skipped": sorted(skipped),
        "reasons": reasons,
    }
