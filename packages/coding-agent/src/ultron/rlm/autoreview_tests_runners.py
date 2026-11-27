"""Test runner detection, command construction, and output parsing for `autoreview_tests`."""
from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass
from typing import Any, Callable

_PY_CONFIG = ("pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg", "conftest.py")
_JS_FILE = re.compile(r"\.(?:[cm]?[jt]s|[jt]sx)$")
_MISSING = re.compile(
    r"ModuleNotFoundError|No module named|ImportError while (importing|loading)|Cannot find module"
    r"|command not found|: not found|npm ERR! missing|ERR_MODULE_NOT_FOUND"
    r"|could not determine executable|no required module provides|cannot find package"
    r"|error: no matching package|failed to select a version|errors? during collection"
    r"|Could not resolve dependencies|network is unreachable"
    r"|Temporary failure in name resolution|getaddrinfo",
    re.I,
)
_PYTEST = re.compile(r"^(PASSED|FAILED|ERROR)\s+(\S+)(?:\s+-\s+(.*))?$", re.M)
_GO = re.compile(r"^\s*--- (PASS|FAIL): (\S+)", re.M)
_CARGO = re.compile(r"^test (\S+) \.\.\. (ok|FAILED)", re.M)


def _pytest_command(_name: str, paths: list[str], select: str | None) -> list[str]:
    select_args = ["-k", select] if select else []
    return [
        "python3",
        "-m",
        "pytest",
        "-q",
        "-rA",
        "--no-header",
        "-p",
        "no:cacheprovider",
        "--tb=short",
        *select_args,
        *paths,
    ]


def _vitest_or_jest_command(name: str, paths: list[str], select: str | None) -> list[str]:
    run_args = ["run"] if name == "vitest" else []
    select_args = ["-t", select] if select else []
    return ["npx", "--no-install", name, *run_args, *select_args, *paths]


def _js_package_command(name: str, paths: list[str], _select: str | None) -> list[str]:
    if paths and name != "yarn":
        return [name, "test", "--", *paths]
    return [name, "test", *paths]


def _go_command(_name: str, paths: list[str], select: str | None) -> list[str]:
    dirs = list(dict.fromkeys("./" + (os.path.dirname(path) or ".") for path in paths))
    targets = dirs if dirs else ["./..."]
    select_args = ["-run", select] if select else []
    return ["go", "test", *select_args, *targets]


def _cargo_command(_name: str, _paths: list[str], select: str | None) -> list[str]:
    select_args = [select] if select else []
    return ["cargo", "test", "--offline", *select_args]


_RUNNER_BUILDERS = {
    "pytest": _pytest_command,
    "vitest": _vitest_or_jest_command,
    "jest": _vitest_or_jest_command,
    "npm": _js_package_command,
    "pnpm": _js_package_command,
    "yarn": _js_package_command,
    "go": _go_command,
    "cargo": _cargo_command,
}


@dataclass
class TestRunner:
    name: str
    #: The files that made the choice, for the record.
    because: str

    def command(self, paths: list[str], select: str | None) -> list[str]:
        builder = _RUNNER_BUILDERS.get(self.name)
        if builder is not None:
            return builder(self.name, paths, select)
        return ["make", "test"]


def _read_package_json(read: Callable[[str], str | None], manifest: str) -> dict[str, Any]:
    raw = read(manifest)
    if not raw:
        return {}
    try:
        parsed = json.loads(raw)
    except ValueError as error:
        if not error.args:
            return {}
        return {}
    return parsed if isinstance(parsed, dict) else {}


def _detect_js_manager(tracked: set[str]) -> str:
    if "pnpm-lock.yaml" in tracked:
        return "pnpm"
    if "yarn.lock" in tracked:
        return "yarn"
    return "npm"


def _package_deps_and_script(package: dict[str, Any]) -> tuple[dict[str, Any], str]:
    dev_deps = package.get("devDependencies")
    deps_map = package.get("dependencies")
    deps: dict[str, Any] = {}
    if isinstance(dev_deps, dict):
        deps.update(dev_deps)
    if isinstance(deps_map, dict):
        deps.update(deps_map)

    scripts = package.get("scripts")
    test_script = scripts.get("test") if isinstance(scripts, dict) else ""
    script = str(test_script) if test_script else ""
    return deps, script


def _package_runner(
    tracked: set[str],
    read: Callable[[str], str | None],
    directory: str,
) -> TestRunner | None:
    """The test runner a package.json declares: vitest or jest, else its `test` script."""
    manifest = f"{directory}/package.json" if directory else "package.json"
    package = _read_package_json(read, manifest)
    deps, script = _package_deps_and_script(package)

    for name in ("vitest", "jest"):
        if name in deps or name in script:
            return TestRunner(name, f"{manifest} ({name})")

    if script and "no test specified" not in script:
        root = directory.split("/")[0] if directory else ""
        manager = _detect_js_manager(tracked)
        suffix = f" ({root})" if root else ""
        return TestRunner(manager, f"{manifest} scripts.test{suffix}")
    return None


def _has_pytest_config(files: set[str], read: Callable[[str], str | None]) -> bool:
    if "pytest.ini" in files or "conftest.py" in files:
        return True
    markers = (
        ("pyproject.toml", "[tool.pytest"),
        ("tox.ini", "pytest"),
        ("setup.cfg", "[tool:pytest]"),
    )
    for filename, needle in markers:
        if filename in files and needle in (read(filename) or ""):
            return True
    return False


def _detect_manifest_runner(
    files: set[str],
    read: Callable[[str], str | None],
) -> TestRunner | None:
    if "go.mod" in files:
        return TestRunner("go", "go.mod")
    if "Cargo.toml" in files:
        return TestRunner("cargo", "Cargo.toml")
    if "Makefile" in files and re.search(r"^test:", read("Makefile") or "", re.M):
        return TestRunner("make", "Makefile test target")
    if any(re.search(r"(^|/)(test_[^/]+|[^/]+_test)\.py$", path) for path in files):
        return TestRunner("pytest", "python test files")
    return None


def detect_runner(
    tracked: list[str] | set[str],
    read: Callable[[str], str | None],
) -> TestRunner | None:
    """Detect the project's test runner from its tracked files, or None."""
    files = set(tracked)
    if _has_pytest_config(files, read):
        return TestRunner("pytest", "pytest configuration")
    if "package.json" in files:
        runner = _package_runner(files, read, "")
        if runner is not None:
            return runner
    return _detect_manifest_runner(files, read)


def _nearest(tracked: set[str], path: str, names: tuple[str, ...]) -> str | None:
    """Closest directory at or above `path` holding one of `names` ("" is root)."""
    directory = os.path.dirname(path)
    while True:
        if any((f"{directory}/{name}" if directory else name) in tracked for name in names):
            return directory
        if not directory:
            return None
        directory = os.path.dirname(directory)


def _nearest_package_runner(
    path: str,
    tracked: set[str],
    read: Callable[[str], str | None],
) -> tuple[TestRunner, str] | None:
    directory = os.path.dirname(path)
    while True:
        manifest = f"{directory}/package.json" if directory else "package.json"
        if manifest in tracked:
            runner = _package_runner(tracked, read, directory)
            if runner is not None:
                return runner, directory
        if not directory:
            return None
        directory = os.path.dirname(directory)


def runner_for(
    path: str,
    tracked: set[str],
    read: Callable[[str], str | None],
) -> tuple[TestRunner, str] | None:
    """Determine the runner and working directory for a single test file."""
    if path.endswith(".py"):
        nearest_dir = _nearest(tracked, path, _PY_CONFIG)
        return TestRunner("pytest", "a Python test file"), nearest_dir if nearest_dir else ""
    if _JS_FILE.search(path):
        return _nearest_package_runner(path, tracked, read)
    if path.endswith(".go"):
        directory = _nearest(tracked, path, ("go.mod",))
        return None if directory is None else (TestRunner("go", "go.mod"), directory)
    if path.endswith(".rs"):
        directory = _nearest(tracked, path, ("Cargo.toml",))
        return None if directory is None else (TestRunner("cargo", "Cargo.toml"), directory)
    return None


def _parse_pytest_tests(output: str) -> list[dict[str, str]]:
    tests: list[dict[str, str]] = []
    for status, name, detail in _PYTEST.findall(output):
        entry = {"id": name, "status": status.lower()}
        if detail:
            entry["detail"] = detail[:200]
        tests.append(entry)
    return tests


def _parse_go_tests(output: str) -> list[dict[str, str]]:
    return [
        {"id": name, "status": "passed" if status == "PASS" else "failed"}
        for status, name in _GO.findall(output)
    ]


def _parse_cargo_tests(output: str) -> list[dict[str, str]]:
    return [
        {"id": name, "status": "passed" if status == "ok" else "failed"}
        for name, status in _CARGO.findall(output)
    ]


def _parse_runner_tests(runner: str, output: str) -> list[dict[str, str]]:
    if runner == "pytest":
        return _parse_pytest_tests(output)
    if runner == "go":
        return _parse_go_tests(output)
    if runner == "cargo":
        return _parse_cargo_tests(output)
    return []


def _is_unavailable_run(
    runner: str,
    code: int,
    output: str,
    tests: list[dict[str, str]],
) -> bool:
    if code == 127:
        return True
    has_failed = any(item["status"] == "failed" for item in tests)
    if has_failed:
        return False
    if _MISSING.search(output):
        return True
    return runner == "pytest" and code in (4, 5)


def parse_outcome(runner: str, code: int, output: str) -> tuple[str, list[dict[str, str]]]:
    """Return `(status, per_test_results)` for a test run."""
    tests = _parse_runner_tests(runner, output)
    if code == 124:
        return "timeout", tests
    if code == 0:
        return "passed", tests
    if _is_unavailable_run(runner, code, output, tests):
        return "unavailable", tests
    return "failed", tests
