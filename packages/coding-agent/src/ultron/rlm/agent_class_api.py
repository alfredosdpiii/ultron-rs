"""Agents as Python classes (NOOA-style) on Ultron's typed agent host."""
from __future__ import annotations

from typing import Any

from agent_class_ast import _bundle_defined_names
from agent_class_builder import _build
from agent_class_runtime import (
    MAX_ARG_BYTES as MAX_ARG_BYTES,
    PREVIEW_BYTES as PREVIEW_BYTES,
    SPILL_SCHEMA as SPILL_SCHEMA,
    STATE_KEY as STATE_KEY,
    AgentCallError as AgentCallError,
    _bucket,
    _ensure_registered,
    _exec_bundle,
    _key_of,
    _state_root,
    begin_cell as begin_cell,
    configure as configure,
    end_cell as end_cell,
    flush as flush,
    get_live,
    get_namespace,
    get_restore_errors,
    rehydrate as rehydrate,
)
from agent_class_schema import (
    MARKER as MARKER,
    _public_schema,
    describe as describe,
    from_json as from_json,
    schema_errors as schema_errors,
    schema_for as schema_for,
    to_json as to_json,
)


def _assign_init_fields(
    instance: Agent,
    spec: Any,
    key: str,
    fields: dict[str, Any],
    created: bool,
) -> None:
    for name, value in fields.items():
        if name not in spec.fields:
            raise TypeError(f"{spec.name} has no field {name!r}")
        setattr(instance, name, value)

    if not created:
        return
    missing = [
        name for name, field in spec.fields.items() if not field.has_default and name not in fields
    ]
    if missing:
        raise TypeError(f"{spec.name}({key!r}) is missing required field(s): {', '.join(missing)}")


def _restore_namespace(
    namespace: dict[str, Any],
    names: set[str],
    saved: dict[str, Any],
) -> None:
    for name in names:
        if name in saved:
            namespace[name] = saved[name]
        else:
            namespace.pop(name, None)


def _load_generation_from_source(spec: Any, source: str, version: int) -> type:
    namespace = get_namespace()
    node_names = _bundle_defined_names(source)
    saved = {name: namespace[name] for name in node_names if name in namespace}
    try:
        old = _exec_bundle(source, "at", version)
    finally:
        _restore_namespace(namespace, node_names, saved)

    if old is None:
        raise LookupError(f"generation {version} of {spec.name} did not define the class")
    return old


class Agent:
    """Base class for agents defined as Python classes. See `agent`."""

    def __init_subclass__(cls, **options: Any) -> None:
        origin = options.pop("_agent_origin", None)
        super().__init_subclass__()
        _build(cls, options, origin, Agent)

    def __init__(self, key: str = "default", **fields: Any) -> None:
        if type(key) is not str or not key.strip() or len(key) > 200:
            raise ValueError("agent key must be a nonempty string of at most 200 characters")

        spec = type(self).__agent__
        self.__dict__["_agent_key"] = key
        root = _state_root()
        assert root is not None
        created = key not in root["instances"].get(spec.id, {})
        _bucket(spec, key, create=True)
        try:
            _assign_init_fields(self, spec, key, fields, created)
        except BaseException:
            if created:
                root["instances"][spec.id].pop(key, None)
                get_live().pop((spec.id, key), None)
            raise

    @property
    def key(self) -> str:
        return _key_of(self)

    def fields(self) -> dict[str, Any]:
        """Current field values (live Python values)."""
        return {name: getattr(self, name) for name in type(self).__agent__.fields}

    def delete(self) -> None:
        """Remove this instance's durable state."""
        spec = type(self).__agent__
        key = _key_of(self)
        root = _state_root()
        assert root is not None
        root["instances"].get(spec.id, {}).pop(key, None)
        get_live().pop((spec.id, key), None)

    def __repr__(self) -> str:
        spec = type(self).__agent__
        return f"<{spec.name} agent key={_key_of(self)!r} generation={spec.generation}>"

    @classmethod
    def definitions(cls) -> dict[str, dict[str, Any]]:
        """Derived definition descriptors for all model-driven methods."""
        spec = cls.__agent__
        version = spec.generation or 0
        return {name: spec.descriptor(method, version) for name, method in spec.methods.items()}

    @classmethod
    async def register(cls) -> int | None:
        """Register the class's definitions now; returns its generation."""
        return await _ensure_registered(cls)

    @classmethod
    def describe(cls) -> dict[str, Any]:
        spec = cls.__agent__
        summary: dict[str, Any] = {
            "name": spec.name,
            "id": spec.id,
            "generation": spec.generation,
            "pinned": spec.pinned is not None,
            "restorable": spec.restorable,
            "fields": {name: describe(field.schema) for name, field in spec.fields.items()},
            "methods": {
                name: {
                    "definition": method.definition_id,
                    "input": method.input_description,
                    "output": describe(_public_schema(method.output_schema)),
                }
                for name, method in spec.methods.items()
            },
        }
        if not spec.restorable:
            summary["not_restorable_because"] = spec.reason
        return summary

    @classmethod
    def generations(cls) -> dict[int, dict[str, Any]]:
        """Recorded generations of this class: `{version: {current, restorable}}`."""
        spec = cls.__agent__
        root = _state_root()
        assert root is not None
        entry = root["classes"].get(spec.id)
        entry_dict = entry if isinstance(entry, dict) else {}
        current = entry_dict.get("generation")
        generations = entry_dict.get("generations", {})
        return {
            int(version): {"current": int(version) == current, "restorable": bool(source)}
            for version, source in sorted(generations.items(), key=lambda item: int(item[0]))
        }

    @classmethod
    def at(cls, version: int) -> type:
        """Return this agent class as it existed at generation `version`."""
        if type(version) is not int or version < 1:
            raise ValueError("generation must be a positive integer")

        spec = cls.__agent__
        root = _state_root()
        assert root is not None
        entry = root["classes"].get(spec.id)
        entry_dict = entry if isinstance(entry, dict) else {}
        source = entry_dict.get("generations", {}).get(str(version))
        if source:
            return _load_generation_from_source(spec, source, version)

        return type(
            cls.__name__,
            (cls,),
            {"__doc__": cls.__doc__, "__module__": cls.__module__, "_agent_pinned_class": version},
        )

    @classmethod
    async def rollback(cls, version: int) -> type:
        """Make generation `version` current again."""
        spec = cls.__agent__
        root = _state_root()
        assert root is not None
        entry = root["classes"].get(spec.id)
        entry_dict = entry if isinstance(entry, dict) else {}
        source = entry_dict.get("generations", {}).get(str(version))
        if not source:
            raise LookupError(f"{spec.name} has no recorded source for generation {version}")

        restored = _exec_bundle(source, "rollback", version)
        if restored is None:
            raise LookupError(f"generation {version} of {spec.name} did not define the class")

        generation = await _ensure_registered(restored)
        if generation != version:
            raise RuntimeError(
                f"{spec.name} rollback to {version} derived generation {generation}; "
                "its definitions changed"
            )
        return restored

    @classmethod
    def instances(cls) -> list[str]:
        """Keys of this class's stored instances."""
        root = _state_root()
        assert root is not None
        return sorted(root["instances"].get(cls.__agent__.id, {}))

    @classmethod
    def load(cls, key: str) -> Any:
        """Return stored instance `key` or raise KeyError."""
        root = _state_root()
        assert root is not None
        if key not in root["instances"].get(cls.__agent__.id, {}):
            raise KeyError(f"{cls.__agent__.name} has no instance {key!r}")
        return cls(key)


class _AgentDecorator:
    """`@agent` / `@agent(name=..., model="provider/model", timeout_ms=..., max_arg_bytes=...)`."""

    AgentCallError = AgentCallError

    def __call__(self, cls: type | None = None, **options: Any) -> Any:
        if cls is None:
            return lambda target: self(target, **options)
        if not isinstance(cls, type):
            raise TypeError("@agent decorates a class")
        if issubclass(cls, Agent):
            if not options:
                return cls
            return type(
                cls.__name__,
                (cls,),
                {"__doc__": cls.__doc__, "__module__": cls.__module__},
                **options,
            )

        namespace = {
            "__doc__": cls.__doc__,
            "__module__": cls.__module__,
            "__qualname__": cls.__qualname__,
        }
        return type(cls.__name__, (cls, Agent), namespace, _agent_origin=cls, **options)

    @staticmethod
    def schema(annotation: Any) -> dict[str, Any]:
        """JSON schema for an annotation, as agent classes derive it."""
        return schema_for(annotation)

    @staticmethod
    def classes() -> dict[str, dict[str, Any]]:
        root = _state_root(create=False)
        classes = root["classes"] if root is not None else {}
        restore_errors = get_restore_errors()
        result: dict[str, dict[str, Any]] = {}
        for class_id, entry in classes.items():
            summary: dict[str, Any] = {
                "name": entry.get("name"),
                "generation": entry.get("generation"),
                "generations": sorted(int(version) for version in entry.get("generations", {})),
                "restorable": bool(entry.get("source")),
            }
            if class_id in restore_errors:
                summary["error"] = restore_errors[class_id]
            result[class_id] = summary
        return result

    def __repr__(self) -> str:
        return "<agent decorator: @agent class Name: ...>"


agent = _AgentDecorator()
