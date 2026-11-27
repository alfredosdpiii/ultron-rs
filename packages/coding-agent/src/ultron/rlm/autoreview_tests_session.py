"""Test session lifecycle, mutation testing, regression comparison, and summaries."""
from __future__ import annotations

import os
import re
import shutil
import time
from pathlib import Path
from typing import Any, Callable

from autoreview_tests_env import discover_environment, interpreter_home
from autoreview_tests_runners import TestRunner, detect_runner, parse_outcome, runner_for
from autoreview_tests_sandbox import (
    DEFAULT_RUNS,
    DEFAULT_TIMEOUT_S,
    MAX_PATHS,
    MAX_REPLACEMENT_CHARS,
    MAX_SELECT_CHARS,
    NODE_ENV,
    OUTPUT_TAIL_CHARS,
    Executor,
    Sandbox,
    TestsRejected,
    _launch_env,
    export_commit,
    run_process,
    sandbox_env,
)


def _valid_toolchains(toolchain: list[str] | None) -> list[str]:
    if not toolchain:
        return []
    valid: list[str] = []
    for path in toolchain:
        if isinstance(path, str) and os.path.isdir(path) and os.path.isdir(os.path.join(path, "bin")):
            valid.append(path)
    return valid


def _resolve_existing_dir(path: str | None) -> str | None:
    if path and os.path.isdir(path):
        return path
    return None


def _resolve_env_kind(
    env_kind: str | None,
    env_dir: str | None,
    checkout: str | None,
) -> str:
    if env_kind:
        return env_kind
    if env_dir is not None:
        return "testEnv"
    if checkout is not None:
        return "checkout"
    return "none"


def _is_safe_relpath(item: Any) -> bool:
    if not isinstance(item, str) or not item:
        return False
    if item.startswith(("/", "-", "~", ":")):
        return False
    if "\0" in item or "\\" in item:
        return False
    return ".." not in item.split("/")


def _validate_test_path(item: Any, tracked: set[str]) -> str:
    if not _is_safe_relpath(item):
        raise TestsRejected(f"{item!r} is not a path inside the repository")
    path = os.path.normpath(item)
    if path in tracked:
        return path
    prefix = path + "/"
    if any(name.startswith(prefix) for name in tracked):
        return path
    raise TestsRejected(f"{path} is not a tracked file or directory at the reviewed commit")


def _validate_select(select: str | None) -> None:
    if select is None:
        return
    valid_text = isinstance(select, str) and len(select) <= MAX_SELECT_CHARS
    if not valid_text or not re.fullmatch(r"[\w .:\[\]()/,=-]+", select):
        raise TestsRejected("select must be a short test name expression")


def _validate_run_request(
    session: TestSession,
    groups: list[tuple[TestRunner, str, list[str]]],
    paths: list[str],
    select: str | None,
) -> tuple[TestRunner, str, list[str]]:
    if not groups:
        raise TestsRejected("no test runner is known for " + ", ".join(paths[:3]))
    if len(groups) > 1:
        summary = "; ".join(
            f"{runner.name} in {directory or '.'}" for runner, directory, _ in groups
        )
        raise TestsRejected(
            f"these paths belong to different test runners ({summary}): ask for them separately"
        )
    _validate_select(select)
    if len(session.records) >= session.limit:
        raise TestsRejected(f"the limit of {session.limit} test executions per review is reached")
    return groups[0]


def _build_base_run_env(
    session: TestSession,
) -> tuple[dict[str, str], list[str | tuple[str, str]]]:
    extra_dirs = [os.path.join(path, "bin") for path in session.toolchain]
    has_env_bin = bool(session.env_dir and os.path.isdir(os.path.join(session.env_dir, "bin")))
    if session.env_dir and has_env_bin:
        extra_dirs.append(os.path.join(session.env_dir, "bin"))

    env = sandbox_env(":".join(extra_dirs) if extra_dirs else None)
    if session.env_dir and not has_env_bin:
        env["NODE_PATH"] = session.env_dir

    binds: list[str | tuple[str, str]] = [session.env_dir] if session.env_dir else []
    binds += list(session.toolchain)
    return env, binds


def _attach_node_runtime(
    session: TestSession,
    runner_name: str,
    binds: list[str | tuple[str, str]],
    env: dict[str, str],
) -> None:
    if runner_name not in ("vitest", "jest", "npm", "pnpm", "yarn"):
        return
    if any(os.path.exists(os.path.join(path, "bin", "node")) for path in session.toolchain):
        return

    node = shutil.which("node")
    prefix = interpreter_home(node) if node else None
    if prefix and prefix not in binds:
        binds.append(prefix)
        env["PATH"] = os.path.join(prefix, "bin") + ":" + env["PATH"]


def _patch_runner_command(
    checkout: str,
    runner_name: str,
    command: list[str],
    found: dict[str, Any],
    workdir: str,
) -> None:
    if runner_name == "pytest" and found["python"]:
        command[0] = os.path.join(workdir, found["python"])
        return
    if runner_name not in ("vitest", "jest"):
        return

    for _source, relative in found["binds"]:
        binary = os.path.join(checkout, relative, ".bin", runner_name)
        if relative.endswith(NODE_ENV) and os.path.lexists(binary):
            command[:3] = [os.path.join(workdir, relative, ".bin", runner_name)]
            break


def _attach_checkout_env(
    session: TestSession,
    runner_name: str,
    directory: str,
    workdir: str,
    command: list[str],
    binds: list[str | tuple[str, str]],
    env: dict[str, str],
) -> list[str]:
    if not session.checkout or session.sandbox.mechanism == "unshare":
        return []

    found = discover_environment(session.checkout, directory)
    binds += [(source, os.path.join(workdir, relative)) for source, relative in found["binds"]]
    binds += found["interpreters"]
    _attach_node_runtime(session, runner_name, binds, env)
    _patch_runner_command(session.checkout, runner_name, command, found, workdir)
    return [relative for _source, relative in found["binds"]]


def _prefix_test_ids(tests: list[dict[str, str]], directory: str) -> list[dict[str, str]]:
    if not directory:
        return tests
    return [{**item, "id": f"{directory}/{item['id']}"} for item in tests]


def _build_run_record(
    session: TestSession,
    *,
    kind: str,
    which: str,
    runner_name: str,
    directory: str,
    command: list[str],
    paths: list[str],
    status: str,
    code: int,
    tests: list[dict[str, str]],
    elapsed_ms: int,
    used: list[str],
    output: str,
    workdir: str,
) -> dict[str, Any]:
    toolchain_labels = [
        f"{os.path.basename(os.path.dirname(path))}/{os.path.basename(path)}"
        for path in session.toolchain
    ]
    return {
        "n": len(session.records) + 1,
        "kind": kind,
        "rev": which,
        "runner": runner_name,
        "cwd": directory or ".",
        "command": " ".join(command),
        "paths": paths,
        "status": status,
        "exit": code,
        "tests": tests[:60],
        "passed": sum(1 for item in tests if item["status"] == "passed"),
        "failed": sum(1 for item in tests if item["status"] in ("failed", "error")),
        "ms": elapsed_ms,
        "environment": used,
        "toolchain": toolchain_labels,
        "output": output[-OUTPUT_TAIL_CHARS:].replace(workdir, "."),
    }


def _validate_mutation_args(
    session: TestSession,
    raw_path: Any,
    replacement: Any,
    tests: Any,
) -> tuple[str, list[str]]:
    if not isinstance(raw_path, str):
        raise TestsRejected("mutation_check needs the path of a tracked file")
    [path] = session.paths([raw_path])
    if path not in session.tracked:
        raise TestsRejected("mutation_check needs the path of a tracked file")

    invalid_replacement = (
        not isinstance(replacement, str)
        or "\n" in replacement
        or "\0" in replacement
        or len(replacement) > MAX_REPLACEMENT_CHARS
    )
    if invalid_replacement:
        raise TestsRejected(
            f"replacement must be one line of at most {MAX_REPLACEMENT_CHARS} characters"
        )

    test_paths = session.paths(tests)
    if len(session.records) >= session.limit:
        raise TestsRejected(f"the limit of {session.limit} test executions per review is reached")
    return path, test_paths


def _parse_mutation_line(
    target: Path,
    path: str,
    line: Any,
) -> tuple[bytes, list[str], int]:
    original = target.read_bytes()
    lines = original.decode("utf-8", "replace").splitlines(keepends=True)
    try:
        number = int(line)
    except (TypeError, ValueError):
        raise TestsRejected("mutation_check needs a line number") from None

    if b"\0" in original[:8192] or not 1 <= number <= len(lines):
        raise TestsRejected(f"{path} has no line {line}")
    return original, lines, number


def _apply_line_mutation(lines: list[str], number: int, replacement: str) -> tuple[str, str]:
    target_line = lines[number - 1]
    if target_line.endswith("\r\n"):
        ending = "\r\n"
    elif target_line.endswith("\n"):
        ending = "\n"
    else:
        ending = ""
    was = target_line.rstrip("\r\n")
    lines[number - 1] = replacement + ending
    return was, "".join(lines)


def _run_base_for_comparison(
    session: TestSession,
    group: list[str],
) -> tuple[dict[str, Any] | None, str | None]:
    try:
        return session.run(group, which="base", kind="base"), None
    except (TestsRejected, RuntimeError) as error:
        return None, str(error)


def _diff_test_outcomes(
    head: dict[str, Any],
    base: dict[str, Any],
    group: list[str],
) -> list[dict[str, Any]]:
    failing = [item for item in head["tests"] if item["status"] in ("failed", "error")]
    passed_before = {item["id"] for item in base["tests"] if item["status"] == "passed"}
    regressed = [item for item in failing if item["id"] in passed_before]
    if base["status"] == "passed" and not head["tests"]:
        regressed = [{"id": " ".join(group), "status": "failed"}]
    return [{**item, "head": head, "base": base} for item in regressed]


def _compare_group(session: TestSession, group: list[str], out: dict[str, Any]) -> None:
    head = session.run(group, kind="automatic")
    out["runs"].append(head)
    if head["status"] != "failed" or not session.revs["base"]:
        return
    if len(session.records) >= session.limit:
        return

    base, _reason = _run_base_for_comparison(session, group)
    if base is not None:
        out["regressions"] += _diff_test_outcomes(head, base, group)


class TestSession:
    """Bounded, sandboxed test executions for one review, each inside a commit export."""

    def __init__(
        self,
        root: str,
        head: str,
        base: str | None,
        tracked: list[str],
        read: Callable[[str], str | None],
        sandbox: Sandbox,
        *,
        runs: int = DEFAULT_RUNS,
        timeout_s: float = DEFAULT_TIMEOUT_S,
        env_dir: str | None = None,
        checkout: str | None = None,
        executor: Executor = run_process,
        export: Callable[[str, str], str] = export_commit,
        clock: Callable[[], float] = time.monotonic,
        toolchain: list[str] | None = None,
        env_kind: str | None = None,
    ) -> None:
        self.root = root
        self.revs = {"head": head, "base": base}
        self.tracked = set(tracked)
        self.sandbox = sandbox
        self.limit = max(0, runs)
        self.timeout_s = timeout_s

        self.toolchain = _valid_toolchains(toolchain)
        valid_env_dir = _resolve_existing_dir(env_dir)
        valid_checkout = _resolve_existing_dir(checkout)
        self.env_kind = _resolve_env_kind(env_kind, valid_env_dir, valid_checkout)
        self.env_dir = valid_env_dir
        self.checkout = valid_checkout if valid_env_dir is None else None

        self._read = read
        self.runner = detect_runner(tracked, read)
        self._executor = executor
        self._export = export
        self._clock = clock
        self._dirs: dict[str, str] = {}
        self.records: list[dict[str, Any]] = []

    def close(self) -> None:
        for path in self._dirs.values():
            shutil.rmtree(path, ignore_errors=True)
        self._dirs.clear()

    def _dir(self, which: str) -> str:
        if which not in self._dirs:
            rev = self.revs[which]
            if not rev:
                raise TestsRejected("the base commit is not known")
            self._dirs[which] = self._export(self.root, rev)
        return self._dirs[which]

    def paths(self, raw: Any) -> list[str]:
        """Return validated, tracked test paths or raise TestsRejected."""
        items = raw if isinstance(raw, list) else []
        if not items or len(items) > MAX_PATHS:
            raise TestsRejected(f"give 1 to {MAX_PATHS} test paths")
        return [_validate_test_path(item, self.tracked) for item in items]

    def plan(self, paths: list[str]) -> list[tuple[TestRunner, str, list[str]]]:
        """Group `paths` by `(runner, directory, relative_paths)`."""
        groups: dict[tuple[str, str], tuple[TestRunner, str, list[str]]] = {}
        for path in paths:
            found = runner_for(path, self.tracked, self._read) if path in self.tracked else None
            is_untracked_or_other = (
                path not in self.tracked or not re.search(r"\.(py|[cm]?[jt]sx?|go|rs)$", path)
            )
            if found is None and is_untracked_or_other and self.runner is not None:
                found = (self.runner, "")
            if found is None:
                continue

            runner, directory = found
            relative = os.path.relpath(path, directory) if directory else path
            groups.setdefault((runner.name, directory), (runner, directory, []))[2].append(relative)
        return list(groups.values())

    def run(
        self,
        paths: list[str],
        select: str | None = None,
        *,
        which: str = "head",
        kind: str = "run",
    ) -> dict[str, Any]:
        """Execute one sandboxed test run on `paths` against the review's budget."""
        groups = self.plan(paths)
        runner, directory, relative = _validate_run_request(self, groups, paths, select)
        workdir = self._dir(which)
        command = runner.command(relative, select)
        env, binds = _build_base_run_env(self)
        used = _attach_checkout_env(self, runner.name, directory, workdir, command, binds, env)

        argv = self.sandbox.wrap(command, workdir, env, binds, directory)
        began = self._clock()
        code, output = self._executor(argv, workdir, _launch_env(), self.timeout_s)
        elapsed_ms = int((self._clock() - began) * 1000)
        status, tests = parse_outcome(runner.name, code, output)
        tests = _prefix_test_ids(tests, directory)

        record = _build_run_record(
            self,
            kind=kind,
            which=which,
            runner_name=runner.name,
            directory=directory,
            command=command,
            paths=paths,
            status=status,
            code=code,
            tests=tests,
            elapsed_ms=elapsed_ms,
            used=used,
            output=output,
            workdir=workdir,
        )
        self.records.append(record)
        return record

    def mutation(
        self,
        raw_path: Any,
        line: Any,
        replacement: Any,
        tests: Any,
    ) -> dict[str, Any]:
        """Replace one line of a tracked file in the head export, run `tests`, and restore."""
        path, test_paths = _validate_mutation_args(self, raw_path, replacement, tests)
        target = Path(self._dir("head"), path)
        original, lines, number = _parse_mutation_line(target, path, line)
        was, mutated_text = _apply_line_mutation(lines, number, replacement)
        try:
            target.write_text(mutated_text, encoding="utf-8")
            record = self.run(test_paths, kind="mutation")
        finally:
            target.write_bytes(original)

        record["mutation"] = {
            "path": path,
            "line": number,
            "was": was[:200],
            "replacement": replacement,
        }
        record["caught"] = record["status"] == "failed"
        return record

    def compare(self, paths: list[str]) -> dict[str, Any]:
        """Run `paths` at head and compare failures against base to find regressions."""
        out: dict[str, Any] = {"runs": [], "regressions": []}
        for _runner, directory, relative in self.plan(paths):
            if len(self.records) >= self.limit:
                break
            group = [os.path.join(directory, item) if directory else item for item in relative]
            _compare_group(self, group, out)
        return out


def _mutation_summary(record: dict[str, Any]) -> str:
    change = record["mutation"]
    if record["caught"]:
        verdict = "the tests caught it"
    elif record["status"] == "passed":
        verdict = "the tests still pass (nothing pins this line)"
    else:
        verdict = "no verdict"
    return (
        f"  with {change['path']}:{change['line']} changed from `{change['was']}` to "
        f"`{change['replacement']}`: {verdict}"
    )


def _failure_detail_lines(record: dict[str, Any]) -> list[str]:
    lines: list[str] = []
    non_passing = [item for item in record["tests"] if item["status"] != "passed"]
    for item in non_passing[:8]:
        detail = f" - {item['detail']}" if item.get("detail") else ""
        lines.append(f"  {item['status']}: {item['id']}{detail}")
    if record["status"] in ("failed", "timeout", "unavailable"):
        tail = "\n".join("    " + line for line in record["output"].splitlines()[-25:])
        lines.append("  output (end):\n" + tail)
    return lines


def summarize(record: dict[str, Any]) -> str:
    """Summarize one run for an investigator or the brief."""
    where = "" if record.get("cwd", ".") == "." else f" in {record['cwd']}"
    counts = (
        f" ({record['passed']} passed, {record['failed']} failed)"
        if record["tests"]
        else ""
    )
    header = (
        f"run {record['n']} ({record['kind']}, at the {record['rev']} commit, "
        f"sandboxed, no network): `{record['command']}`{where} -> {record['status']}{counts}"
    )
    lines = [header]
    if record.get("mutation"):
        lines.append(_mutation_summary(record))
    if record["status"] == "unavailable":
        lines.append(
            "  the tests could not run (missing dependencies or nothing collected; the sandbox "
            "has no network and nothing is installed): this says nothing about the code"
        )
    lines.extend(_failure_detail_lines(record))
    return "\n".join(lines)
