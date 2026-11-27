"""Environment discovery and interpreter prefix resolution for `autoreview_tests`."""
from __future__ import annotations

import os
from typing import Any

from autoreview_tests_sandbox import NODE_ENV, PYTHON_ENVS

_SYSTEM_PREFIXES = ("/usr/", "/bin/", "/sbin/", "/lib/", "/lib64/", "/lib32/")


def _valid_interpreter_prefix(real: str, home_path: str) -> str | None:
    if not os.path.isfile(real) or real.startswith(_SYSTEM_PREFIXES):
        return None
    bin_dir = os.path.dirname(real)
    if os.path.basename(bin_dir) != "bin":
        return None

    prefix = os.path.dirname(bin_dir)
    if not os.path.isdir(os.path.join(prefix, "lib")):
        return None
    if prefix in ("/", home_path) or home_path.startswith(prefix + "/") or prefix.count("/") < 3:
        return None
    return prefix


def interpreter_home(binary: str, home: str | None = None) -> str | None:
    """Resolve the install directory of a non-system Python interpreter, or None."""
    if not binary:
        return None
    try:
        real = os.path.realpath(binary)
    except OSError as error:
        if error.errno is None:
            return None
        return None

    resolved_home = home if home is not None else os.path.expanduser("~")
    home_path = os.path.realpath(resolved_home)
    return _valid_interpreter_prefix(real, home_path)


def _inside(path: str, root: str) -> bool:
    real = os.path.realpath(path)
    return real == root or real.startswith(root + "/")


def _find_python_env(
    base: str,
    current: str,
    root: str,
    home: str | None,
) -> tuple[tuple[str, str], str, str | None] | None:
    for name in PYTHON_ENVS:
        env = os.path.join(base, name)
        python = os.path.join(env, "bin", "python")
        if not (os.path.isdir(env) and _inside(env, root) and os.path.lexists(python)):
            continue

        relative = os.path.join(current, name) if current else name
        prefix = interpreter_home(python, home)
        external_prefix = prefix if prefix and not _inside(prefix, root) else None
        return (os.path.realpath(env), relative), os.path.join(relative, "bin", "python"), external_prefix
    return None


def _discover_level(
    root: str,
    current: str,
    home: str | None,
    out: dict[str, Any],
) -> None:
    base = os.path.join(root, current) if current else root
    if out["python"] is None:
        found_py = _find_python_env(base, current, root, home)
        if found_py is not None:
            bind, python_path, external_prefix = found_py
            out["binds"].append(bind)
            out["python"] = python_path
            if external_prefix is not None:
                out["interpreters"].append(external_prefix)

    modules = os.path.join(base, NODE_ENV)
    if os.path.isdir(modules) and _inside(modules, root):
        relative = os.path.join(current, NODE_ENV) if current else NODE_ENV
        out["binds"].append((os.path.realpath(modules), relative))


def discover_environment(
    checkout: str,
    directory: str = "",
    home: str | None = None,
) -> dict[str, Any]:
    """Find the nearest virtualenv and every `node_modules` from `directory` up to `checkout`."""
    out: dict[str, Any] = {"binds": [], "python": None, "interpreters": []}
    root = os.path.realpath(checkout)
    if not os.path.isdir(root):
        return out

    current = directory.strip("/")
    while True:
        _discover_level(root, current, home, out)
        if not current:
            break
        current = os.path.dirname(current)
    return out


def _prune_env_dirs(dirs: list[str], level: int, depth: int) -> list[str]:
    env_names = (*PYTHON_ENVS, NODE_ENV)
    found = [name for name in dirs if name in env_names]
    if level >= depth:
        dirs[:] = []
    else:
        dirs[:] = [name for name in dirs if name not in found and not name.startswith(".")]
    return sorted(found)


def _env_entry(
    current: str,
    relative: str,
    name: str,
    root: str,
) -> dict[str, Any] | None:
    path = os.path.join(current, name)
    if not _inside(path, root):
        return None

    rel_path = name if relative == "." else os.path.join(relative, name)
    if name == NODE_ENV:
        return {"path": rel_path, "kind": "node_modules"}

    python = os.path.join(path, "bin", "python")
    if not os.path.lexists(python):
        return None

    prefix = interpreter_home(python)
    interpreter = prefix if prefix and not _inside(prefix, root) else "system"
    return {"path": rel_path, "kind": "virtualenv", "interpreter": interpreter}


def list_environments(checkout: str, depth: int = 3) -> list[dict[str, Any]]:
    """List every prepared environment of a checkout down to `depth` directories."""
    root = os.path.realpath(checkout)
    out: list[dict[str, Any]] = []
    for current, dirs, _files in os.walk(root):
        relative = os.path.relpath(current, root)
        level = 0 if relative == "." else relative.count("/") + 1
        found = _prune_env_dirs(dirs, level, depth)
        for name in found:
            entry = _env_entry(current, relative, name, root)
            if entry is not None:
                out.append(entry)
    return out
