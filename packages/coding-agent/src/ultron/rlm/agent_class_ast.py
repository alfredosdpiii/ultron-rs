"""AST inspection, source bundling, and naming utilities for agent classes."""
from __future__ import annotations

import ast
import inspect
import re
import sys
from typing import Any

_TYPE_ALIAS_NODE = getattr(ast, "TypeAlias", ())


def _kebab(name: str) -> str:
    text = re.sub(r"(?<=[a-z0-9])([A-Z])", r"-\1", name)
    text = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1-\2", text).lower()
    text = re.sub(r"[^a-z0-9]+", "-", text).strip("-")
    text = re.sub(r"^[^a-z]+", "", text)
    return text or "agent"


def _own_annotation_names(base: type) -> list[str]:
    """Names a class annotates itself, without evaluating the annotations."""
    try:
        import annotationlib

        return list(annotationlib.get_annotations(base, format=annotationlib.Format.FORWARDREF))
    except ImportError:
        return list(base.__dict__.get("__annotations__", {}))


def _is_ellipsis_body(node: ast.AST) -> bool:
    body = list(getattr(node, "body", []))
    if body and isinstance(body[0], ast.Expr):
        expr_val = getattr(body[0], "value", None)
        if isinstance(expr_val, ast.Constant) and isinstance(expr_val.value, str):
            body = body[1:]
    if len(body) != 1 or not isinstance(body[0], ast.Expr):
        return False
    return isinstance(body[0].value, ast.Constant) and body[0].value.value is Ellipsis


def _looks_like_agent(node: ast.ClassDef) -> bool:
    for decorator in node.decorator_list:
        target = decorator.func if isinstance(decorator, ast.Call) else decorator
        if isinstance(target, ast.Name) and target.id == "agent":
            return True
    return any(isinstance(base, ast.Name) and base.id == "Agent" for base in node.bases)


def _names_used(node: ast.AST) -> set[str]:
    return {item.id for item in ast.walk(node) if isinstance(item, ast.Name)}


def _simple_value(node: ast.AST | None) -> bool:
    if node is None:
        return True
    disallowed = (ast.Call, ast.Await, ast.Lambda, ast.NamedExpr, ast.Yield)
    return not any(isinstance(item, disallowed) for item in ast.walk(node))


def _assign_target_names(node: ast.Assign) -> set[str]:
    if not _simple_value(node.value):
        return set()
    return {target.id for target in node.targets if isinstance(target, ast.Name)}


def _defined_names(node: ast.stmt) -> set[str]:
    if isinstance(node, ast.ClassDef | ast.FunctionDef | ast.AsyncFunctionDef):
        return {node.name}
    if isinstance(node, _TYPE_ALIAS_NODE) and isinstance(node.name, ast.Name):
        return {node.name.id}
    if isinstance(node, ast.Assign):
        return _assign_target_names(node)
    if isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
        return {node.target.id} if _simple_value(node.value) else set()
    return set()


def _segment_with_decorators(source: str, node: ast.stmt) -> str:
    lines = source.splitlines()
    first = min([node.lineno, *(item.lineno for item in getattr(node, "decorator_list", []))])
    last = node.end_lineno or node.lineno
    return "\n".join(lines[first - 1 : last])


def _node_start_line(node: ast.ClassDef) -> int:
    return min([node.lineno, *(item.lineno for item in node.decorator_list)])


def _load_source_tree(
    cls: type,
    cell_source: str | None,
    rehydrating: dict[str, Any] | None,
) -> tuple[str | None, ast.Module | None, str]:
    source = rehydrating["source"] if rehydrating is not None else cell_source
    if source is None:
        try:
            source = inspect.getsource(sys.modules[cls.__module__])
        except (OSError, TypeError, KeyError):
            return None, None, "class source is unavailable"
    try:
        tree = ast.parse(source)
    except SyntaxError:
        return None, None, "class source does not parse"
    return source, tree, ""


def _matching_class_nodes(
    nodes: Any,
    name: str,
    first: int | None,
) -> list[ast.ClassDef]:
    matches: list[ast.ClassDef] = []
    for node in nodes:
        if not isinstance(node, ast.ClassDef) or node.name != name:
            continue
        if first is None or _node_start_line(node) == first:
            matches.append(node)
    return matches


def _find_class_node(tree: ast.Module, cls: type) -> tuple[ast.ClassDef | None, str]:
    first = getattr(cls, "__firstlineno__", None)
    matches = _matching_class_nodes(tree.body, cls.__name__, first)
    if matches:
        return matches[-1], ""

    nested = _matching_class_nodes(ast.walk(tree), cls.__name__, first)
    if nested:
        return nested[-1], "class is not defined at the top level of a cell"
    return None, "class source was not found"


def _collect_providers(earlier: list[ast.stmt]) -> dict[str, ast.stmt]:
    providers: dict[str, ast.stmt] = {}
    for node in earlier:
        if isinstance(node, ast.ClassDef) and _looks_like_agent(node):
            continue
        for name in _defined_names(node):
            providers[name] = node
    return providers


def _transitive_dependencies(
    target: ast.ClassDef,
    providers: dict[str, ast.stmt],
) -> list[ast.stmt]:
    needed: list[ast.stmt] = []
    pending = _names_used(target)
    seen: set[str] = set()
    while pending:
        name = pending.pop()
        if name in seen or name not in providers:
            continue
        seen.add(name)
        node = providers[name]
        if node not in needed:
            needed.append(node)
            pending |= _names_used(node)
    return needed


def _build_class_bundle(source: str, tree: ast.Module, target: ast.ClassDef) -> str:
    index = tree.body.index(target)
    earlier = tree.body[:index]
    providers = _collect_providers(earlier)
    needed = _transitive_dependencies(target, providers)
    imports = [node for node in earlier if isinstance(node, ast.Import | ast.ImportFrom)]
    chosen = [node for node in earlier if node in imports or node in needed] + [target]
    return "\n\n".join(_segment_with_decorators(source, node) for node in chosen) + "\n"


def _class_source(
    cls: type,
    cell_source: str | None,
    rehydrating: dict[str, Any] | None,
) -> tuple[ast.ClassDef | None, str | None, str]:
    """Return `(class_node, restorable_bundle_source, reason)` for a cell or module class."""
    source, tree, error = _load_source_tree(cls, cell_source, rehydrating)
    if source is None or tree is None:
        return None, None, error

    target, find_error = _find_class_node(tree, cls)
    if target is None or find_error:
        return target, None, find_error
    if rehydrating is not None:
        return target, source, ""
    return target, _build_class_bundle(source, tree, target), ""


def _ellipsis_method_names(node: ast.ClassDef | None, class_name: str) -> set[str]:
    if node is None:
        return set()
    names: set[str] = set()
    for item in node.body:
        if not isinstance(item, ast.FunctionDef | ast.AsyncFunctionDef):
            continue
        if not _is_ellipsis_body(item):
            continue
        if isinstance(item, ast.FunctionDef):
            raise TypeError(f"{class_name}.{item.name}: a model-driven method must be `async def`")
        names.add(item.name)
    return names


def _bundle_defined_names(source: str) -> set[str]:
    names: set[str] = set()
    for node in ast.parse(source).body:
        names |= _defined_names(node)
        for alias in getattr(node, "names", []):
            names.add(alias.asname or alias.name.split(".")[0])
    return names
