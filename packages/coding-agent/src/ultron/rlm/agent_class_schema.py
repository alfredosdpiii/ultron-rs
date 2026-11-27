"""JSON schema derivation, validation, serialization, and rendering for agent classes."""
from __future__ import annotations

import collections.abc
import dataclasses
import enum
import inspect
import json
import math
import re
import types
import typing
from typing import Any

MARKER = "x-ultron-agent-class"
_MISSING = object()
_NoneType = type(None)
_REQUIRED = getattr(typing, "Required", _MISSING)
_NOT_REQUIRED = getattr(typing, "NotRequired", _MISSING)

_PRIMITIVE_TYPES: tuple[tuple[type, str], ...] = (
    (bool, "boolean"),
    (int, "integer"),
    (float, "number"),
    (str, "string"),
)
_SEQUENCE_ORIGINS = (
    list,
    collections.abc.Sequence,
    collections.abc.MutableSequence,
    collections.abc.Iterable,
    collections.abc.Collection,
)
_SET_ORIGINS = (set, frozenset, collections.abc.Set, collections.abc.MutableSet)
_MAPPING_ORIGINS = (dict, collections.abc.Mapping, collections.abc.MutableMapping)

_SCHEMA_MAPS = {"properties", "patternProperties", "$defs", "definitions", "dependentSchemas"}
_SCHEMA_ONE = {
    "items",
    "additionalItems",
    "additionalProperties",
    "unevaluatedProperties",
    "unevaluatedItems",
    "propertyNames",
    "contains",
    "not",
    "if",
    "then",
    "else",
}
_SCHEMA_LISTS = {"allOf", "anyOf", "oneOf", "prefixItems"}
_SCHEMA_VALUES = {
    "type",
    "enum",
    "const",
    "required",
    "title",
    "description",
    "format",
    "pattern",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "uniqueItems",
    "examples",
    "readOnly",
    "writeOnly",
    "deprecated",
    "dependentRequired",
    "minContains",
    "maxContains",
    "contentEncoding",
    "contentMediaType",
}
_EXACT_TYPE_MAP = {"boolean": bool, "string": str, "array": list, "object": dict}

_namespace_getter: Any = None


def set_namespace_getter(getter: Any) -> None:
    global _namespace_getter
    _namespace_getter = getter


def get_namespace() -> dict[str, Any]:
    if _namespace_getter is None:
        return {}
    return _namespace_getter()


def _is_empty_dict(value: Any) -> bool:
    return isinstance(value, dict) and len(value) == 0


def _is_pydantic(tp: Any) -> bool:
    return (
        isinstance(tp, type)
        and callable(getattr(tp, "model_json_schema", None))
        and callable(getattr(tp, "model_validate", None))
    )


def _json_literal(value: Any, where: str) -> Any:
    if isinstance(value, enum.Enum):
        value = value.value
    is_primitive = type(value) in (str, int, bool)
    is_finite_float = type(value) is float and math.isfinite(value)
    if value is None or is_primitive or is_finite_float:
        return value
    raise TypeError(f"{where}: Literal/Enum value {value!r} is not a JSON primitive")


def _eval_mro_annotations(tp: Any, namespace: dict[str, Any]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for base in reversed(getattr(tp, "__mro__", (tp,))):
        if base is object or base is dict:
            continue
        for name, raw in inspect.get_annotations(base).items():
            value = raw.__forward_arg__ if isinstance(raw, typing.ForwardRef) else raw
            result[name] = eval(value, namespace) if isinstance(value, str) else value  # noqa: S307
    return result


def _hints(tp: Any) -> dict[str, Any]:
    """Resolve annotations in the kernel namespace rather than `__main__` when applicable."""
    namespace = _namespace_getter() if _namespace_getter else None
    if namespace is None or getattr(tp, "__module__", None) != namespace.get("__name__"):
        return typing.get_type_hints(tp, include_extras=True)
    try:
        return typing.get_type_hints(tp, globalns=namespace, include_extras=True)
    except NameError:
        return _eval_mro_annotations(tp, namespace)


def _annotation_text(annotation: Any) -> str:
    if annotation is inspect.Parameter.empty:
        return ""
    namespace = _namespace_getter() if _namespace_getter else {}
    base_module = namespace.get("__name__", "__main__")
    formatted = inspect.formatannotation(annotation, base_module=base_module)
    return formatted.replace("__main__.", "")


def _public_schema(schema: Any) -> Any:
    if isinstance(schema, dict):
        return {key: _public_schema(value) for key, value in schema.items() if key != MARKER}
    if isinstance(schema, list):
        return [_public_schema(item) for item in schema]
    return schema


def _schema_primitive(tp: Any) -> dict[str, Any] | None:
    if tp is Any or tp is object or tp is inspect.Parameter.empty:
        return {}
    if tp is None or tp is _NoneType:
        return {"type": "null"}
    for target, kind in _PRIMITIVE_TYPES:
        if tp is target:
            return {"type": kind}
    return None


def _annotated_schema(args: tuple[Any, ...], where: str, stack: tuple[Any, ...]) -> dict[str, Any]:
    inner = _schema(args[0], where, stack)
    notes = [item for item in args[1:] if isinstance(item, str)]
    if notes:
        return {**inner, "description": " ".join(notes)}
    return inner


def _union_schema(args: tuple[Any, ...], where: str, stack: tuple[Any, ...]) -> dict[str, Any]:
    members = [_schema(arg, where, stack) for arg in args]
    if any(_is_empty_dict(member) for member in members):
        return {}
    return {"anyOf": members}


def _tuple_schema(args: tuple[Any, ...], where: str, stack: tuple[Any, ...]) -> dict[str, Any]:
    if len(args) == 2 and args[1] is Ellipsis:
        return {"type": "array", "items": _schema(args[0], where, stack)}
    if args == ((),) or not args:
        return {"type": "array", "maxItems": 0}
    return {
        "type": "array",
        "prefixItems": [_schema(arg, where, stack) for arg in args],
        "minItems": len(args),
        "maxItems": len(args),
        "items": False,
    }


def _mapping_schema(args: tuple[Any, ...], where: str, stack: tuple[Any, ...]) -> dict[str, Any]:
    if not args:
        return {"type": "object"}
    if args[0] not in (str, Any):
        raise TypeError(f"{where}: dict keys must be str for JSON, got {args[0]!r}")
    return {"type": "object", "additionalProperties": _schema(args[1], where, stack)}


def _collection_origin_schema(
    tp: Any,
    origin: Any,
    args: tuple[Any, ...],
    where: str,
    stack: tuple[Any, ...],
) -> dict[str, Any] | None:
    if tp in (list, tuple, set, frozenset) or origin in _SEQUENCE_ORIGINS:
        schema: dict[str, Any] = {"type": "array"}
        if args:
            schema["items"] = _schema(args[0], where, stack)
        if tp in (set, frozenset):
            schema["uniqueItems"] = True
        return schema
    if origin in _SET_ORIGINS:
        item_schema = _schema(args[0], where, stack) if args else {}
        return {"type": "array", "items": item_schema, "uniqueItems": True}
    if origin is tuple:
        return _tuple_schema(args, where, stack)
    if tp is dict or origin in _MAPPING_ORIGINS:
        return _mapping_schema(args, where, stack)
    return None


def _origin_schema(
    tp: Any,
    origin: Any,
    args: tuple[Any, ...],
    where: str,
    stack: tuple[Any, ...],
) -> dict[str, Any] | None:
    if origin is typing.Annotated:
        return _annotated_schema(args, where, stack)
    if origin in (_REQUIRED, _NOT_REQUIRED):
        return _schema(args[0], where, stack)
    if origin is typing.Literal:
        values = [_json_literal(value, where) for value in args]
        return {"const": values[0]} if len(values) == 1 else {"enum": values}
    if origin is typing.Union or origin is types.UnionType:
        return _union_schema(args, where, stack)
    return _collection_origin_schema(tp, origin, args, where, stack)


def _dataclass_schema(tp: type, where: str, stack: tuple[Any, ...]) -> dict[str, Any]:
    hints = _hints(tp)
    properties: dict[str, Any] = {}
    required: list[str] = []
    next_stack = (*stack, tp)
    for item in dataclasses.fields(tp):
        if not item.init:
            continue
        properties[item.name] = _schema(hints.get(item.name, Any), f"{where}.{item.name}", next_stack)
        if item.default is dataclasses.MISSING and item.default_factory is dataclasses.MISSING:
            required.append(item.name)

    schema: dict[str, Any] = {
        "type": "object",
        "title": tp.__name__,
        "properties": properties,
        "additionalProperties": False,
    }
    if required:
        schema["required"] = required
    return schema


def _is_required_td_field(hint: Any, total: bool) -> bool:
    origin = typing.get_origin(hint)
    if origin is _REQUIRED:
        return True
    return total and origin is not _NOT_REQUIRED


def _typeddict_schema(tp: type, where: str, stack: tuple[Any, ...]) -> dict[str, Any]:
    hints = _hints(tp)
    total = getattr(tp, "__total__", True)
    next_stack = (*stack, tp)
    required_keys = [name for name, hint in hints.items() if _is_required_td_field(hint, total)]
    properties = {
        name: _schema(hint, f"{where}.{name}", next_stack) for name, hint in hints.items()
    }
    schema: dict[str, Any] = {
        "type": "object",
        "title": tp.__name__,
        "properties": properties,
        "additionalProperties": False,
    }
    if required_keys:
        schema["required"] = required_keys
    return schema


def _type_schema(tp: type, where: str, stack: tuple[Any, ...]) -> dict[str, Any]:
    if issubclass(tp, enum.Enum):
        return {"enum": [_json_literal(member.value, where) for member in tp]}
    if tp in stack:
        raise TypeError(f"{where}: recursive type {tp.__name__} is not supported")
    if dataclasses.is_dataclass(tp):
        return _dataclass_schema(tp, where, stack)
    if typing.is_typeddict(tp):
        return _typeddict_schema(tp, where, stack)
    if _is_pydantic(tp):
        return _sanitize(_inline_refs(tp.model_json_schema(), where), where)
    raise TypeError(
        f"{where}: unsupported annotation {getattr(tp, '__name__', tp)!r}; use str, int, float, "
        "bool, None, list, dict[str, T], tuple, set, Optional, Union, Literal, Enum, "
        "a dataclass, a TypedDict, or a pydantic model"
    )


def _schema(tp: Any, where: str, stack: tuple[Any, ...]) -> dict[str, Any]:
    primitive = _schema_primitive(tp)
    if primitive is not None:
        return primitive
    if isinstance(tp, str):
        raise TypeError(f"{where}: unresolved string annotation {tp!r}; define the type before the class")

    origin = typing.get_origin(tp)
    args = typing.get_args(tp)
    derived = _origin_schema(tp, origin, args, where, stack)
    if derived is not None:
        return derived
    if not isinstance(tp, type):
        raise TypeError(f"{where}: unsupported annotation {tp!r}")
    return _type_schema(tp, where, stack)


def schema_for(tp: Any, where: str = "value") -> dict[str, Any]:
    """Derive the JSON schema for a Python annotation."""
    return _schema(tp, where, ())


def _resolve_schema_ref(
    node: dict[str, Any],
    ref: str,
    definitions: dict[str, Any],
    where: str,
    active: tuple[str, ...],
) -> Any:
    name = ref.rsplit("/", 1)[-1]
    if not ref.startswith("#/") or name not in definitions:
        raise TypeError(f"{where}: unsupported schema reference {ref!r}")
    if name in active:
        raise TypeError(f"{where}: recursive model {name} is not supported")
    inlined = _walk_schema_node(definitions[name], definitions, where, (*active, name))
    overrides = {key: val for key, val in node.items() if key != "$ref"}
    return _walk_schema_node({**inlined, **overrides}, definitions, where, active)


def _walk_schema_node(
    node: Any,
    definitions: dict[str, Any],
    where: str,
    active: tuple[str, ...],
) -> Any:
    if isinstance(node, list):
        return [_walk_schema_node(item, definitions, where, active) for item in node]
    if not isinstance(node, dict):
        return node
    ref = node.get("$ref")
    if isinstance(ref, str):
        return _resolve_schema_ref(node, ref, definitions, where, active)
    return {
        key: _walk_schema_node(value, definitions, where, active)
        for key, value in node.items()
        if key not in ("$defs", "definitions")
    }


def _inline_refs(schema: Any, where: str) -> Any:
    """Replace local `#/$defs/X` references because the host registry refuses `$ref`."""
    definitions: dict[str, Any] = {}
    if isinstance(schema, dict):
        raw_defs = schema.get("definitions")
        raw_dollar_defs = schema.get("$defs")
        if isinstance(raw_defs, dict):
            definitions.update(raw_defs)
        if isinstance(raw_dollar_defs, dict):
            definitions.update(raw_dollar_defs)
    return _walk_schema_node(schema, definitions, where, ())


def _sanitize_keyword(key: str, value: Any, where: str) -> tuple[bool, Any]:
    if key in _SCHEMA_MAPS:
        return True, {name: _sanitize(item, where) for name, item in value.items()}
    if key in _SCHEMA_ONE:
        return True, _sanitize(value, where)
    if key in _SCHEMA_LISTS:
        return True, [_sanitize(item, where) for item in value]
    if key in _SCHEMA_VALUES or key.startswith("x-"):
        return True, value
    return False, None


def _sanitize(schema: Any, where: str) -> Any:
    """Keep only JSON Schema keywords accepted by the host definition registry."""
    if isinstance(schema, bool):
        return schema
    if not isinstance(schema, dict):
        raise TypeError(f"{where}: schema must be an object")

    result: dict[str, Any] = {}
    for key, value in schema.items():
        keep, sanitized = _sanitize_keyword(key, value, where)
        if keep:
            result[key] = sanitized
    return result


def _json_key(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def _type_ok(kind: str, value: Any) -> bool:
    exact = _EXACT_TYPE_MAP.get(kind)
    if exact is not None:
        return type(value) is exact
    if kind == "null":
        return value is None
    if kind == "integer":
        return type(value) is int or (type(value) is float and value.is_integer())
    if kind == "number":
        return type(value) in (int, float)
    return False


def _check_node_type(node: dict[str, Any], item: Any, at: str, errors: list[str]) -> bool:
    kind = node.get("type")
    if kind is None:
        return True
    kinds = kind if isinstance(kind, list) else [kind]
    if any(_type_ok(entry, item) for entry in kinds):
        return True
    errors.append(f"{at} must be {' or '.join(kinds)}, got {type(item).__name__}")
    return False


def _check_combinators(
    node: dict[str, Any],
    item: Any,
    at: str,
    errors: list[str],
    limit: int,
) -> None:
    for keyword in ("anyOf", "oneOf"):
        if keyword in node and not any(
            not schema_errors(member, item, at, 1) for member in node[keyword]
        ):
            detail = "; ".join(schema_errors(node[keyword][-1], item, at, 1))
            errors.append(f"{at} matches none of the allowed shapes ({detail})")
    for member in node.get("allOf", []):
        _check_node(member, item, at, errors, limit)


def _check_node_values(
    node: dict[str, Any],
    item: Any,
    at: str,
    errors: list[str],
    limit: int,
) -> None:
    if "const" in node and _json_key(node["const"]) != _json_key(item):
        errors.append(f"{at} must equal {_json_key(node['const'])}")
    if "enum" in node and _json_key(item) not in {_json_key(entry) for entry in node["enum"]}:
        choices = ", ".join(_json_key(entry) for entry in node["enum"])
        errors.append(f"{at} must be one of {choices}")
    _check_combinators(node, item, at, errors, limit)


def _check_str(node: dict[str, Any], item: str, at: str, errors: list[str]) -> None:
    if "minLength" in node and len(item) < node["minLength"]:
        errors.append(f"{at} must have at least {node['minLength']} characters")
    if "maxLength" in node and len(item) > node["maxLength"]:
        errors.append(f"{at} must have at most {node['maxLength']} characters")
    if "pattern" in node and not re.search(node["pattern"], item):
        errors.append(f"{at} must match /{node['pattern']}/")


def _check_number(node: dict[str, Any], item: int | float, at: str, errors: list[str]) -> None:
    bounds = (
        ("minimum", item >= node["minimum"]) if "minimum" in node else None,
        ("maximum", item <= node["maximum"]) if "maximum" in node else None,
        ("exclusiveMinimum", item > node["exclusiveMinimum"]) if "exclusiveMinimum" in node else None,
        ("exclusiveMaximum", item < node["exclusiveMaximum"]) if "exclusiveMaximum" in node else None,
    )
    for entry in bounds:
        if entry is not None and not entry[1]:
            errors.append(f"{at} must satisfy {entry[0]} {node[entry[0]]}")


def _check_array_items(
    node: dict[str, Any],
    item: list[Any],
    at: str,
    errors: list[str],
    limit: int,
) -> None:
    prefix = node.get("prefixItems", [])
    for index, entry in enumerate(item):
        if index < len(prefix):
            _check_node(prefix[index], entry, f"{at}[{index}]", errors, limit)
        elif "items" in node:
            _check_node(node["items"], entry, f"{at}[{index}]", errors, limit)


def _check_array(
    node: dict[str, Any],
    item: list[Any],
    at: str,
    errors: list[str],
    limit: int,
) -> None:
    if "minItems" in node and len(item) < node["minItems"]:
        errors.append(f"{at} must have at least {node['minItems']} items")
    if "maxItems" in node and len(item) > node["maxItems"]:
        errors.append(f"{at} must have at most {node['maxItems']} items")
    if node.get("uniqueItems") and len({_json_key(entry) for entry in item}) != len(item):
        errors.append(f"{at} must not contain duplicates")
    _check_array_items(node, item, at, errors, limit)


def _check_object(
    node: dict[str, Any],
    item: dict[str, Any],
    at: str,
    errors: list[str],
    limit: int,
) -> None:
    properties = node.get("properties", {})
    for name in node.get("required", []):
        if name not in item:
            errors.append(f"{at}.{name} is required")

    for name, entry in item.items():
        if name in properties:
            _check_node(properties[name], entry, f"{at}.{name}", errors, limit)
        elif "additionalProperties" in node:
            extra = node["additionalProperties"]
            if extra is False:
                errors.append(f"{at}.{name} is not an allowed property")
            else:
                _check_node(extra, entry, f"{at}.{name}", errors, limit)


def _check_by_python_type(
    node: dict[str, Any],
    item: Any,
    at: str,
    errors: list[str],
    limit: int,
) -> None:
    if type(item) is str:
        _check_str(node, item, at, errors)
    elif type(item) in (int, float) and type(item) is not bool:
        _check_number(node, item, at, errors)
    elif type(item) is list:
        _check_array(node, item, at, errors, limit)
    elif type(item) is dict:
        _check_object(node, item, at, errors, limit)


def _check_node(node: Any, item: Any, at: str, errors: list[str], limit: int) -> None:
    if len(errors) >= limit or node is True or _is_empty_dict(node):
        return
    if node is False:
        errors.append(f"{at} is not allowed")
        return
    if not _check_node_type(node, item, at, errors):
        return
    _check_node_values(node, item, at, errors, limit)
    _check_by_python_type(node, item, at, errors, limit)


def schema_errors(schema: Any, value: Any, path: str = "$", limit: int = 5) -> list[str]:
    """Validate `value` against the schema subset derived by `schema_for`."""
    errors: list[str] = []
    _check_node(schema, value, path, errors, limit)
    return errors[:limit]


def _scalar_to_json(value: Any, where: str) -> tuple[bool, Any]:
    if value is None or type(value) in (bool, int, str):
        return True, value
    if isinstance(value, float):
        if not math.isfinite(value):
            raise TypeError(f"{where}: {value!r} is not JSON")
        return True, float(value)
    if isinstance(value, enum.Enum):
        return True, to_json(value.value, where)
    # Check bool before int because bool subclasses int in Python.
    if isinstance(value, bool):
        return True, bool(value)
    if isinstance(value, int):
        return True, int(value)
    if isinstance(value, str):
        return True, str(value)
    return False, None


def _dict_to_json(value: dict[Any, Any], where: str) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise TypeError(f"{where}: dict key {key!r} is not a string")
        result[str(key)] = to_json(item, f"{where}.{key}")
    return result


def _set_to_json(value: set[Any] | frozenset[Any], where: str) -> list[Any]:
    items = [to_json(item, f"{where}[]") for item in value]
    try:
        return sorted(items)
    except TypeError:
        return sorted(items, key=_json_key)


def _dataclass_to_json(value: Any, where: str) -> dict[str, Any]:
    return {
        item.name: to_json(getattr(value, item.name), f"{where}.{item.name}")
        for item in dataclasses.fields(value)
    }


def _container_to_json(value: Any, where: str) -> Any:
    if isinstance(value, dict):
        return _dict_to_json(value, where)
    if isinstance(value, list | tuple):
        return [to_json(item, f"{where}[{index}]") for index, item in enumerate(value)]
    if isinstance(value, set | frozenset):
        return _set_to_json(value, where)
    raise TypeError(f"{where}: {type(value).__name__} is not JSON data")


def to_json(value: Any, where: str = "value") -> Any:
    """Encode a typed Python value as plain JSON data."""
    handled, scalar = _scalar_to_json(value, where)
    if handled:
        return scalar
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return _dataclass_to_json(value, where)
    if callable(getattr(value, "model_dump", None)) and not isinstance(value, type):
        return value.model_dump(mode="json")
    return _container_to_json(value, where)


def _from_json_literal(args: tuple[Any, ...], value: Any) -> Any:
    target = _json_key(value)
    for choice in args:
        if _json_key(_json_literal(choice, "value")) == target:
            return choice
    return value


def _from_json_union(args: tuple[Any, ...], value: Any) -> Any:
    if value is None and _NoneType in args:
        return None
    for arg in args:
        if arg is not _NoneType and not schema_errors(schema_for(arg), value, limit=1):
            return from_json(arg, value)
    return value


def _from_json_tuple(args: tuple[Any, ...], value: Any) -> tuple[Any, ...]:
    if len(args) == 2 and args[1] is Ellipsis:
        return tuple(from_json(args[0], item) for item in value)
    return tuple(from_json(arg, item) for arg, item in zip(args, value, strict=False))


def _from_json_sequence(args: tuple[Any, ...], value: Any) -> list[Any]:
    elem_type = args[0] if args else Any
    return [from_json(elem_type, item) for item in value]


def _from_json_set(origin: Any, args: tuple[Any, ...], value: Any) -> Any:
    kind = frozenset if origin is frozenset else set
    elem_type = args[0] if args else Any
    return kind(from_json(elem_type, item) for item in value)


def _from_json_mapping(args: tuple[Any, ...], value: Any) -> dict[Any, Any]:
    val_type = args[1] if args else Any
    return {key: from_json(val_type, item) for key, item in value.items()}


def _from_json_collection(origin: Any, args: tuple[Any, ...], value: Any) -> Any:
    if origin in _SEQUENCE_ORIGINS:
        return _from_json_sequence(args, value)
    if origin in _SET_ORIGINS:
        return _from_json_set(origin, args, value)
    if origin is tuple:
        return _from_json_tuple(args, value)
    if origin in _MAPPING_ORIGINS:
        return _from_json_mapping(args, value)
    return _MISSING


_ORIGIN_DECODERS: dict[Any, Any] = {
    typing.Annotated: lambda args, value: from_json(args[0], value),
    typing.Literal: _from_json_literal,
    typing.Union: _from_json_union,
    types.UnionType: _from_json_union,
}


def _from_json_origin(origin: Any, args: tuple[Any, ...], value: Any) -> Any:
    if origin in (_REQUIRED, _NOT_REQUIRED):
        return from_json(args[0], value)
    decoder = _ORIGIN_DECODERS.get(origin)
    if decoder is not None:
        return decoder(args, value)
    return _from_json_collection(origin, args, value)


def _from_json_class(tp: Any, value: Any) -> Any:
    if isinstance(tp, type) and issubclass(tp, enum.Enum):
        return tp(value)
    if dataclasses.is_dataclass(tp):
        hints = _hints(tp)
        kwargs = {
            item.name: from_json(hints.get(item.name, Any), value[item.name])
            for item in dataclasses.fields(tp)
            if item.init and item.name in value
        }
        return tp(**kwargs)
    if typing.is_typeddict(tp):
        hints = _hints(tp)
        return {key: from_json(hints.get(key, Any), item) for key, item in value.items()}
    if _is_pydantic(tp):
        return tp.model_validate(value)
    return value


_PASSTHROUGH_TYPES = (Any, object, inspect.Parameter.empty, None, _NoneType)
_DIRECT_TYPES = (float, bool, int, str, list, tuple, set, frozenset, dict)


def from_json(tp: Any, value: Any) -> Any:
    """Build the annotated Python value from validated JSON data."""
    if any(tp is target for target in _PASSTHROUGH_TYPES):
        return value
    for direct_type in _DIRECT_TYPES:
        if tp is direct_type:
            return direct_type(value)

    origin = typing.get_origin(tp)
    if origin is not None:
        decoded = _from_json_origin(origin, typing.get_args(tp), value)
        if decoded is not _MISSING:
            return decoded
    return _from_json_class(tp, value)


def _checked(schema: dict[str, Any], value: Any, where: str) -> Any:
    encoded = to_json(value, where)
    errors = schema_errors(schema, encoded, where)
    if errors:
        raise TypeError("; ".join(errors))
    return encoded


def _describe_array(schema: dict[str, Any], depth: int) -> str:
    if "prefixItems" in schema:
        items = ", ".join(describe(item, depth + 1) for item in schema["prefixItems"])
        return f"[{items}]"
    inner = describe(schema.get("items", {}), depth + 1)
    return f"({inner})[]" if " " in inner else f"{inner}[]"


def _describe_object(schema: dict[str, Any], depth: int) -> str:
    properties = schema.get("properties")
    if properties:
        required = set(schema.get("required", []))
        rendered = [
            f"{name}{'' if name in required else '?'}: {describe(item, depth + 1)}"
            for name, item in properties.items()
        ]
        return "{" + ", ".join(rendered) + "}"

    extra = schema.get("additionalProperties")
    if isinstance(extra, dict):
        return f"{{[key: string]: {describe(extra, depth + 1)}}}"
    return "object"


def _describe_choice(schema: dict[str, Any], depth: int) -> str | None:
    if "const" in schema:
        return _json_key(schema["const"])
    if "enum" in schema:
        return " | ".join(_json_key(item) for item in schema["enum"])
    for keyword in ("anyOf", "oneOf"):
        if keyword in schema:
            return " | ".join(describe(member, depth + 1) for member in schema[keyword])
    return None


def describe(schema: Any, depth: int = 0) -> str:
    """Render a compact TypeScript-like description of a JSON schema."""
    if schema is True or _is_empty_dict(schema) or depth > 6:
        return "any"
    if schema is False:
        return "never"

    choice = _describe_choice(schema, depth)
    if choice is not None:
        return choice

    kind = schema.get("type")
    if kind == "array":
        return _describe_array(schema, depth)
    if kind == "object":
        return _describe_object(schema, depth)
    return kind if isinstance(kind, str) else "any"
