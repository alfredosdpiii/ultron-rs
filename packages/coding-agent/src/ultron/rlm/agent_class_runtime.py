"""Durable state, host registration, and method invocation runtime for agent classes."""
from __future__ import annotations

import asyncio
import copy
import dataclasses
import hashlib
import inspect
import json
import os
import sys
import tempfile
from pathlib import Path
from typing import Any

from agent_class_schema import (
    MARKER as MARKER,
    _annotation_text,
    _checked,
    _is_empty_dict,
    _json_key,
    _public_schema,
    describe,
    from_json,
    get_namespace,
    schema_errors,
    set_namespace_getter,
    to_json,
)

STATE_KEY = "__agents__"
MAX_ARG_BYTES = 16_384
PREVIEW_BYTES = 2_048
SPILL_SCHEMA: dict[str, Any] = {
    "type": "object",
    "properties": {
        "$preview": {"type": "object"},
        "path": {"type": "string"},
        "sha256": {"type": "string"},
        "bytes": {"type": "integer"},
    },
    "required": ["$preview", "path", "sha256", "bytes"],
    "additionalProperties": False,
}

_MISSING = object()
_bridge: Any = None
_preview: Any = None
_cell_source: str | None = None
_rehydrating: dict[str, Any] | None = None
_live: dict[tuple[str, str], dict[str, tuple[Any, Any]]] = {}
_restore_errors: dict[str, str] = {}


def configure(bridge: Any, namespace_getter: Any, preview: Any) -> None:
    global _bridge, _preview
    _bridge = bridge
    _preview = preview
    set_namespace_getter(namespace_getter)


def get_cell_source() -> str | None:
    return _cell_source


def get_rehydrating() -> dict[str, Any] | None:
    return _rehydrating


def get_live() -> dict[tuple[str, str], dict[str, tuple[Any, Any]]]:
    return _live


def get_restore_errors() -> dict[str, str]:
    return _restore_errors


class AgentCallError(RuntimeError):
    """A model-driven method did not produce a valid typed value."""

    def __init__(self, message: str, result: Any = None, definition: str | None = None) -> None:
        super().__init__(message)
        self.result = result
        self.definition = definition


def _state_root(create: bool = True) -> dict[str, Any] | None:
    namespace = get_namespace()
    state = namespace.get("state")
    if not isinstance(state, dict):
        if not create:
            return None
        raise RuntimeError("agent classes keep their state in `state`, which must be a dict")

    root = state.get(STATE_KEY)
    if root is None:
        if not create:
            return None
        classes: dict[str, Any] = {}
        instances: dict[str, Any] = {}
        state[STATE_KEY] = {"classes": classes, "instances": instances}
        return state[STATE_KEY]
    return root


def _bucket(spec: _AgentSpec, key: str, create: bool = False) -> dict[str, Any] | None:
    root = _state_root()
    assert root is not None
    instances = (
        root["instances"].setdefault(spec.id, {})
        if create
        else root["instances"].get(spec.id, {})
    )
    if key not in instances and create:
        instances[key] = {}
    return instances.get(key)


def flush(raise_errors: bool = False) -> list[str]:
    """Write live field values (including in-place mutations) back into `state` as JSON."""
    problems: list[str] = []
    root = _state_root(create=False)
    if root is None:
        return problems

    for (class_id, key), fields in list(_live.items()):
        bucket = root["instances"].get(class_id, {}).get(key)
        if bucket is None:
            continue
        for name, (field, value) in fields.items():
            try:
                bucket[name] = _checked(field.schema, value, f"{class_id}[{key!r}].{name}")
            except TypeError as error:
                problems.append(str(error))

    if problems and raise_errors:
        raise TypeError("; ".join(problems))
    return problems


class _Field:
    """Durable, typed agent field backed by the instance's state bucket."""

    def __init__(
        self,
        name: str,
        annotation: Any,
        schema: dict[str, Any],
        default: Any,
        factory: Any,
    ) -> None:
        self.name = name
        self.annotation = annotation
        self.schema = schema
        self.default = default
        self.factory = factory
        self.public = not name.startswith("_")

    @property
    def has_default(self) -> bool:
        return self.default is not _MISSING or self.factory is not _MISSING

    def make_default(self) -> Any:
        return self.factory() if self.factory is not _MISSING else copy.deepcopy(self.default)

    def __repr__(self) -> str:
        return f"<agent field {self.name}: {describe(self.schema)}>"

    def __get__(self, obj: Any, owner: Any = None) -> Any:
        if obj is None:
            return self

        spec = type(obj).__agent__
        key = _key_of(obj)
        live = _live.setdefault((spec.id, key), {})
        if self.name in live:
            return live[self.name][1]

        bucket = _bucket(spec, key, create=True)
        assert bucket is not None
        if self.name in bucket:
            try:
                value = from_json(self.annotation, copy.deepcopy(bucket[self.name]))
            except Exception as error:
                desc = describe(self.schema)
                raise TypeError(
                    f"{spec.name}[{key!r}].{self.name}: stored value does not fit {desc}: {error}"
                ) from error
        elif self.has_default:
            value = self.make_default()
            bucket[self.name] = _checked(self.schema, value, f"{spec.name}.{self.name}")
        else:
            raise AttributeError(f"{spec.name}[{key!r}].{self.name} has no value")

        live[self.name] = (self, value)
        return value

    def __set__(self, obj: Any, value: Any) -> None:
        spec = type(obj).__agent__
        key = _key_of(obj)
        encoded = _checked(self.schema, value, f"{spec.name}.{self.name}")
        bucket = _bucket(spec, key, create=True)
        assert bucket is not None
        bucket[self.name] = encoded
        _live.setdefault((spec.id, key), {})[self.name] = (self, value)

    def __delete__(self, obj: Any) -> None:
        raise AttributeError(f"agent field {self.name} cannot be deleted; assign a value instead")


def _key_of(obj: Any) -> str:
    key = obj.__dict__.get("_agent_key")
    if key is None:
        key = obj.__dict__["_agent_key"] = "default"
    return key


@dataclasses.dataclass
class _Method:
    name: str
    definition_id: str
    func: Any
    signature: inspect.Signature
    params: dict[str, tuple[Any, dict[str, Any]]]
    returns: Any
    output_schema: dict[str, Any]
    instructions: str
    input_schema: dict[str, Any]
    input_description: str
    output_description: str


@dataclasses.dataclass
class _AgentSpec:
    name: str
    id: str
    fields: dict[str, _Field]
    methods: dict[str, _Method]
    model: str | None
    timeout_ms: int | None
    max_arg_bytes: int
    bundle: str | None
    restorable: bool
    reason: str
    pinned: int | None = None
    generation: int | None = None
    lock: Any = None

    def descriptor(self, method: _Method, version: int) -> dict[str, Any]:
        result: dict[str, Any] = {
            "id": method.definition_id,
            "version": str(version),
            "strategy": "rlm",
            "instructions": method.instructions,
            "inputSchema": method.input_schema,
            "outputSchema": method.output_schema,
            "maxRepairs": 0,
            "inputDescription": method.input_description,
            "outputDescription": method.output_description,
        }
        if self.model is not None:
            result["model"] = self.model
        return result


def _spillable(schema: dict[str, Any]) -> dict[str, Any]:
    return {} if _is_empty_dict(schema) else {"anyOf": [schema, SPILL_SCHEMA]}


def _record_class(spec: _AgentSpec) -> None:
    if _rehydrating is not None and _rehydrating["mode"] in ("at", "restore"):
        return

    root = _state_root()
    assert root is not None
    entry = root["classes"].get(spec.id)
    max_order = max((item["order"] for item in root["classes"].values()), default=0)
    order = entry["order"] if entry else 1 + max_order
    raw_generations = entry.get("generations") if entry else None
    generations = dict(raw_generations) if isinstance(raw_generations, dict) else {}
    root["classes"][spec.id] = {
        "name": spec.name,
        "source": spec.bundle,
        "reason": spec.reason,
        "order": order,
        "generation": entry.get("generation") if entry else None,
        "generations": generations,
    }


def _spill_dir() -> Path:
    configured = os.environ.get("ULTRON_RLM_SPILL_DIR")
    base = (
        Path(configured)
        if configured
        else Path(tempfile.gettempdir()) / f"ultron-{os.getuid()}" / "agent-args"
    )
    base.mkdir(parents=True, exist_ok=True, mode=0o700)
    return base


def _generic_json(value: Any, where: str) -> Any:
    try:
        return to_json(value, where)
    except TypeError:
        return {"$preview": _preview(value, max_bytes=PREVIEW_BYTES), "path": "", "sha256": "", "bytes": 0}


def _argument(spec: _AgentSpec, value: Any, schema: dict[str, Any], where: str) -> Any:
    encoded = _generic_json(value, where) if _is_empty_dict(schema) else _checked(schema, value, where)
    text = json.dumps(encoded, ensure_ascii=False, separators=(",", ":"))
    size = len(text.encode("utf-8"))
    if size <= spec.max_arg_bytes:
        return encoded

    digest = hashlib.sha256(text.encode("utf-8")).hexdigest()
    path = _spill_dir() / f"{digest}.json"
    if not path.exists():
        temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
        file_desc = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(file_desc, "w", encoding="utf-8") as handle:
            handle.write(text)
        os.replace(temporary, path)
    return {
        "$preview": _preview(value, max_bytes=PREVIEW_BYTES),
        "path": str(path),
        "sha256": digest,
        "bytes": size,
    }


def _build_invoke_payload(
    spec: _AgentSpec,
    obj: Any,
    method: _Method,
    label: str,
    args: tuple[Any, ...],
    kwargs: dict[str, Any],
) -> dict[str, Any]:
    try:
        bound = method.signature.bind(obj, *args, **kwargs)
    except TypeError as error:
        raise TypeError(f"{label}: {error}") from None

    bound.apply_defaults()
    call_args: dict[str, Any] = {}
    for parameter, value in list(bound.arguments.items())[1:]:
        _annotation, schema = method.params[parameter]
        call_args[parameter] = _argument(spec, value, schema, f"{label}({parameter})")

    payload: dict[str, Any] = {"args": call_args}
    public = {key: field for key, field in spec.fields.items() if field.public}
    if public:
        payload["state"] = {
            key: _argument(spec, getattr(obj, key), field.schema, f"{label} state.{key}")
            for key, field in public.items()
        }
    return payload


def _decode_invoke_result(
    method: _Method,
    label: str,
    definition: str,
    result: Any,
) -> Any:
    status = result.get("status") if isinstance(result, dict) else None
    if status != "succeeded":
        error = result.get("error") if isinstance(result, dict) else result
        raise AgentCallError(f"{label} ({definition}) {status or 'failed'}: {error}", result, definition)

    value = result.get("value")
    errors = schema_errors(_public_schema(method.output_schema), value, "$")
    if errors:
        # Host validates too; this prevents a broken host from returning an unchecked value.
        detail = "; ".join(errors)
        raise AgentCallError(
            f"{label} ({definition}) returned a value that does not match its contract: {detail}",
            result,
            definition,
        )
    try:
        return from_json(method.returns, value)
    except Exception as error:
        type_text = _annotation_text(method.returns)
        raise AgentCallError(
            f"{label} ({definition}) returned a value that could not become {type_text}: {error}",
            result,
            definition,
        ) from error


async def _invoke(
    cls: type,
    obj: Any,
    name: str,
    args: tuple[Any, ...],
    kwargs: dict[str, Any],
) -> Any:
    spec: _AgentSpec = cls.__agent__
    method = spec.methods[name]
    label = f"{spec.name}.{name}"
    payload = _build_invoke_payload(spec, obj, method, label, args, kwargs)
    generation = await _ensure_registered(cls)
    definition = f"{method.definition_id}@{generation}"
    request: dict[str, Any] = {"definition": definition, "input": payload}
    if spec.timeout_ms is not None:
        request["timeout_ms"] = spec.timeout_ms

    result = await _bridge.request("agents.invoke", request)
    return _decode_invoke_result(method, label, definition, result)


def _index_existing_definitions(
    listed: Any,
    ids: set[str],
) -> tuple[dict[tuple[str, str], Any], list[int]]:
    existing: dict[tuple[str, str], Any] = {}
    if not listed:
        return existing, []
    for item in listed:
        if isinstance(item, dict) and item.get("id") in ids:
            existing[(item["id"], item["version"])] = item
    versions = sorted({int(version) for (_id, version) in existing}, reverse=True)
    return existing, versions


def _find_matching_version(
    spec: _AgentSpec,
    existing: dict[tuple[str, str], Any],
    versions: list[int],
) -> int | None:
    for version in versions:
        version_str = str(version)
        if all(
            (method.definition_id, version_str) in existing
            and _json_key(existing[(method.definition_id, version_str)])
            == _json_key(spec.descriptor(method, version))
            for method in spec.methods.values()
        ):
            return version
    return None


async def _try_register_once(spec: _AgentSpec, attempt: int) -> int | None:
    listed = await _bridge.request("agents.list")
    ids = {method.definition_id for method in spec.methods.values()}
    existing, versions = _index_existing_definitions(listed, ids)
    chosen = _find_matching_version(spec, existing, versions)
    if chosen is not None:
        return chosen

    candidate = versions[0] + 1 if versions else 1
    try:
        for method in spec.methods.values():
            await _bridge.request("agents.register", {"definition": spec.descriptor(method, candidate)})
    except RuntimeError as error:
        if "hash conflict" in str(error) and attempt < 2:
            return None
        raise
    return candidate


async def _ensure_registered(cls: type) -> int | None:
    spec: _AgentSpec = cls.__agent__
    if spec.generation is not None:
        return spec.generation
    if not spec.methods:
        return None
    if spec.lock is None:
        spec.lock = asyncio.Lock()

    async with spec.lock:
        if spec.generation is not None:
            return spec.generation
        for attempt in range(3):
            chosen = await _try_register_once(spec, attempt)
            if chosen is not None:
                spec.generation = chosen
                break
        assert spec.generation is not None
        _record_generation(spec, spec.generation)
        return spec.generation


def _record_generation(spec: _AgentSpec, version: int) -> None:
    root = _state_root()
    assert root is not None
    entry = root["classes"].get(spec.id)
    if entry is None:
        return
    entry["generation"] = version
    if spec.bundle is not None:
        entry.setdefault("generations", {})[str(version)] = spec.bundle


def _exec_bundle(source: str, mode: str, version: int | None = None) -> type | None:
    global _rehydrating
    previous = _rehydrating
    _rehydrating = {"source": source, "mode": mode, "version": version, "result": None}
    try:
        exec(compile(source, "<agent-class>", "exec"), get_namespace())
        return _rehydrating["result"]
    finally:
        _rehydrating = previous


def begin_cell(source: str) -> None:
    global _cell_source
    _cell_source = source


def end_cell() -> None:
    global _cell_source
    _cell_source = None
    try:
        problems = flush()
    except Exception as error:  # a replaced `state` must not fail the cell
        problems = [str(error)]
    for problem in problems:
        print(f"agent state not saved: {problem}", file=sys.stderr)


def rehydrate() -> None:
    """Re-execute stored agent classes from source after snapshot restore or scratch reset."""
    _live.clear()
    root = _state_root(create=False)
    if root is None:
        return
    namespace = get_namespace()
    classes = sorted(root.get("classes", {}).items(), key=lambda item: item[1].get("order", 0))
    for class_id, entry in classes:
        source = entry.get("source")
        if not source:
            continue
        existing = namespace.get(entry.get("name"))
        if (
            isinstance(existing, type)
            and getattr(existing, "__agent__", None) is not None
            and existing.__agent__.bundle == source
        ):
            continue
        try:
            _exec_bundle(source, "restore")
            _restore_errors.pop(class_id, None)
        except Exception as error:
            _restore_errors[class_id] = f"{type(error).__name__}: {error}"
