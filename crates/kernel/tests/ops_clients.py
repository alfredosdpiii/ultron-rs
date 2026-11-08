"""Lists the host requests the runtime's Python clients send: for each `bridge.request("<op>", payload)` call in
the files named on the command line, the op, the payload keys the call can send and the reply keys it reads.
Prints one JSON object {op: {"payload": [...], "reply": [...], "open": bool, "sites": [...]}}; `open` marks a
payload whose keys cannot all be read from the source (an unknown dict passed through)."""

import ast
import json
import sys


def const_str(node):
    return node.value if isinstance(node, ast.Constant) and isinstance(node.value, str) else None


class Keys:
    def __init__(self):
        self.keys = set()
        self.open = False

    def add(self, other):
        self.keys |= other.keys
        self.open = self.open or other.open


def helper_returns(module):
    """Module-level helpers that build a payload dict from their keyword arguments or a dict literal."""
    helpers = {}
    for node in module.body:
        if isinstance(node, ast.FunctionDef):
            helpers[node.name] = node
    return helpers


def dict_keys(node, scope, helpers, seen):
    keys = Keys()
    if node is None or (isinstance(node, ast.Constant) and node.value is None):
        return keys
    if isinstance(node, ast.Dict):
        for key, value in zip(node.keys, node.values):
            if key is None:
                keys.add(dict_keys(value, scope, helpers, seen))
            elif const_str(key) is not None:
                keys.keys.add(const_str(key))
            else:
                keys.open = True
        return keys
    if isinstance(node, ast.IfExp):
        keys.add(dict_keys(node.body, scope, helpers, seen))
        keys.add(dict_keys(node.orelse, scope, helpers, seen))
        return keys
    if isinstance(node, ast.Call):
        name = node.func.id if isinstance(node.func, ast.Name) else None
        if name == "dict" or name in helpers:
            # `dict(...)` and pass-through helpers (`_compact({...})`, `_payload(a=..)`) send the keys they are
            # given; a helper that builds its own dict sends the keys it returns.
            for keyword in node.keywords:
                if keyword.arg is None:
                    keys.add(dict_keys(keyword.value, scope, helpers, seen))
                else:
                    keys.keys.add(keyword.arg)
            for arg in node.args:
                if isinstance(arg, (ast.Dict, ast.Call)) or name == "dict":
                    keys.add(dict_keys(arg, scope, helpers, seen))
            if name in helpers and name not in seen:
                keys.add(returned_keys(helpers[name], helpers, seen | {name}))
            return keys
        keys.open = True
        return keys
    if isinstance(node, ast.Name):
        keys.add(name_keys(node.id, scope, helpers, seen))
        return keys
    keys.open = True
    return keys


def returned_keys(function, helpers, seen):
    keys = Keys()
    for node in ast.walk(function):
        # A comprehension filters the dict it was given (`{k: v for k, v in payload.items() if v is not None}`).
        if isinstance(node, ast.Return) and node.value is not None and not isinstance(node.value, ast.DictComp):
            keys.add(dict_keys(node.value, function, helpers, seen))
    return keys


def first_returned_keys(function, helpers, seen):
    """The keys of the dict a helper returns first in a tuple (`return payload, mode, wait`)."""
    keys = Keys()
    for node in ast.walk(function):
        if isinstance(node, ast.Return) and isinstance(node.value, ast.Tuple) and node.value.elts:
            keys.add(dict_keys(node.value.elts[0], function, helpers, seen))
    return keys


def loop_keys(name, scope):
    """The constant first items `name` takes in `for name, ... in (("a", ...), ("b", ...))`."""
    keys = set()
    for node in ast.walk(scope):
        if (
            isinstance(node, ast.For)
            and isinstance(node.target, ast.Tuple)
            and node.target.elts
            and isinstance(node.target.elts[0], ast.Name)
            and node.target.elts[0].id == name
            and isinstance(node.iter, (ast.Tuple, ast.List))
        ):
            for item in node.iter.elts:
                if isinstance(item, ast.Tuple) and item.elts and const_str(item.elts[0]) is not None:
                    keys.add(const_str(item.elts[0]))
    return keys


def name_keys(name, scope, helpers, seen):
    keys = Keys()
    assigned = False
    if scope is None:
        keys.open = True
        return keys
    for node in ast.walk(scope):
        if isinstance(node, (ast.Assign, ast.AnnAssign)):
            targets = node.targets if isinstance(node, ast.Assign) else [node.target]
            for target in targets:
                if (
                    isinstance(target, ast.Tuple)
                    and target.elts
                    and isinstance(target.elts[0], ast.Name)
                    and target.elts[0].id == name
                    and isinstance(node.value, ast.Call)
                    and isinstance(node.value.func, ast.Name)
                    and node.value.func.id in helpers
                ):
                    assigned = True
                    keys.add(first_returned_keys(helpers[node.value.func.id], helpers, seen))
                if isinstance(target, ast.Name) and target.id == name and node.value is not None:
                    assigned = True
                    keys.add(dict_keys(node.value, scope, helpers, seen | {name}) if name not in seen else Keys())
                if (
                    isinstance(target, ast.Subscript)
                    and isinstance(target.value, ast.Name)
                    and target.value.id == name
                ):
                    key = const_str(target.slice)
                    if key is not None:
                        keys.keys.add(key)
                    elif isinstance(target.slice, ast.Name) and loop_keys(target.slice.id, scope):
                        keys.keys |= loop_keys(target.slice.id, scope)
                    else:
                        keys.open = True
        if (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and isinstance(node.func.value, ast.Name)
            and node.func.value.id == name
        ):
            if node.func.attr == "update":
                for arg in node.args:
                    keys.add(dict_keys(arg, scope, helpers, seen | {name}))
                for keyword in node.keywords:
                    keys.keys.add(keyword.arg)
            elif node.func.attr == "setdefault" and node.args and const_str(node.args[0]) is not None:
                keys.keys.add(const_str(node.args[0]))
    if not assigned:
        # A parameter or a global: its keys come from elsewhere.
        keys.open = True
    return keys


def reply_keys(call, parents, scope):
    """Keys read straight off the awaited reply: `(await r(...))["k"]`, or `x = await r(...)` then `x["k"]` /
    `x.get("k")`."""
    keys = set()
    node = parents.get(call)
    if isinstance(node, ast.Await):
        outer = parents.get(node)
        if isinstance(outer, ast.Subscript) and outer.value is node and const_str(outer.slice):
            keys.add(const_str(outer.slice))
        if isinstance(outer, ast.Assign) and len(outer.targets) == 1 and isinstance(outer.targets[0], ast.Name):
            name = outer.targets[0].id
            for other in ast.walk(scope) if scope is not None else []:
                if isinstance(other, ast.Subscript) and isinstance(other.value, ast.Name) and other.value.id == name:
                    if const_str(other.slice):
                        keys.add(const_str(other.slice))
                if (
                    isinstance(other, ast.Call)
                    and isinstance(other.func, ast.Attribute)
                    and other.func.attr == "get"
                    and isinstance(other.func.value, ast.Name)
                    and other.func.value.id == name
                    and other.args
                    and const_str(other.args[0])
                ):
                    keys.add(const_str(other.args[0]))
    return keys


def main(paths):
    found = {}
    modules = []
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            modules.append((path, ast.parse(handle.read(), path)))
    # Payload helpers are imported between the client modules (`from payload_fields import given`).
    helpers = {}
    for _, module in modules:
        helpers.update(helper_returns(module))
    for path, module in modules:
        parents = {}
        scopes = {}

        def visit(node, scope):
            for child in ast.iter_child_nodes(node):
                parents[child] = node
                inner = child if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)) else scope
                scopes[child] = inner
                visit(child, inner)

        visit(module, None)
        for node in ast.walk(module):
            if not (
                isinstance(node, ast.Call)
                and isinstance(node.func, ast.Attribute)
                and node.func.attr == "request"
                and node.args
                and const_str(node.args[0]) is not None
            ):
                continue
            op = const_str(node.args[0])
            scope = scopes.get(node)
            payload = dict_keys(node.args[1] if len(node.args) > 1 else None, scope, helpers, frozenset())
            entry = found.setdefault(op, {"payload": set(), "reply": set(), "open": False, "sites": []})
            entry["payload"] |= payload.keys
            entry["open"] = entry["open"] or payload.open
            entry["reply"] |= reply_keys(node, parents, scope)
            entry["sites"].append(f"{path.rsplit('/', 1)[-1]}:{node.lineno}")
    print(json.dumps({
        op: {"payload": sorted(e["payload"]), "reply": sorted(e["reply"]), "open": e["open"], "sites": e["sites"]}
        for op, e in sorted(found.items())
    }, indent=1))


if __name__ == "__main__":
    main(sys.argv[1:])
