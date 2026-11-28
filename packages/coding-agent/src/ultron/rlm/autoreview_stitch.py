"""STITCH++ diff-proving stage for `autoreview` and `/review`."""
from __future__ import annotations

import json
import os
import re
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from autoreview_static_checks import (
    MAX_STAGE_FINDINGS,
    EnvFn,
    Executor,
    Which,
    _finding,
    _new_lines,
    _Stage,
)
from review_api import FileDiff, _text

STITCH_ENV_FLAG = "ULTRON_REVIEW_STITCH"
STITCH_PROBE_SECS_ENV = "ULTRON_REVIEW_STITCH_SECS"
STITCH_DIR_ENV = "ULTRON_STITCH_DIR"
DEFAULT_PROBE_SECS = 20
MIN_PROBE_SECS = 5
MAX_PROBE_SECS = 120
STITCH_CMD_TIMEOUT_S = 180.0


def stitch_ephemeral_dir(name: str = "") -> Path:
    """Dedicated directory outside `/tmp` for ephemeral STITCH++ review harnesses."""
    override = os.environ.get(STITCH_DIR_ENV, "").strip()
    base = Path(override).expanduser() if override else Path.home() / ".ultron" / "stitch" / "ephemeral"
    target = base / name if name else base
    target.mkdir(parents=True, exist_ok=True)
    return target

_CRATE_PATH_RE = re.compile(r"\bcrates/([A-Za-z0-9_-]+)/")
_DISABLED_VALUES = frozenset({"0", "false", "no", "off"})


@dataclass(frozen=True)
class StitchTarget:
    name: str
    directory: str
    stitch_version: str | None
    matched_files: tuple[str, ...]


def _stitch_enabled() -> bool:
    raw = os.environ.get(STITCH_ENV_FLAG, "").strip().lower()
    return raw not in _DISABLED_VALUES


def _probe_seconds() -> int:
    raw = os.environ.get(STITCH_PROBE_SECS_ENV, "").strip()
    if not raw:
        return DEFAULT_PROBE_SECS
    try:
        parsed = int(raw)
    except ValueError:
        return DEFAULT_PROBE_SECS
    return max(MIN_PROBE_SECS, min(MAX_PROBE_SECS, parsed))


def _read_stitch_config(toml_path: Path) -> tuple[str | None, list[str]]:
    try:
        data = tomllib.loads(toml_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError):
        return None, []
    project = data.get("project")
    raw_version = (
        project.get("stitch_version")
        if isinstance(project, dict)
        else data.get("stitch_version")
    )
    stitch_version = (
        raw_version.strip()
        if isinstance(raw_version, str) and raw_version.strip()
        else None
    )
    prefixes: list[str] = []
    rust_cfg = data.get("rust")
    deps = rust_cfg.get("dependencies") if isinstance(rust_cfg, dict) else None
    if isinstance(deps, dict):
        for spec in deps.values():
            dep_path = spec.get("path") if isinstance(spec, dict) else None
            if not isinstance(dep_path, str):
                continue
            normalized = dep_path.removeprefix("/src/").strip("/")
            if normalized:
                prefixes.append(f"{normalized}/")
    raw_targets = data.get("targets")
    if isinstance(raw_targets, list):
        for item in raw_targets:
            if isinstance(item, str) and item.strip():
                prefixes.append(item.strip().strip("/") + "/")
    return stitch_version, prefixes


def _infer_covered_prefixes(harness_dir: Path, name: str, explicit: list[str]) -> tuple[str, ...]:
    prefixes: set[str] = {f"stitch/{name}/", *explicit}
    dockerfile = harness_dir / "Dockerfile"
    if dockerfile.is_file():
        try:
            text = dockerfile.read_text(encoding="utf-8", errors="replace")
        except OSError:
            text = ""
        for crate_dir in _CRATE_PATH_RE.findall(text):
            prefixes.add(f"crates/{crate_dir}/")
    return tuple(sorted(prefixes))


def discover_matching_harnesses(repo_root: str, changed_paths: list[str]) -> list[StitchTarget]:
    """Return every `stitch/<name>/` harness covering at least one changed file."""
    stitch_root = Path(repo_root) / "stitch"
    if not stitch_root.is_dir():
        return []

    targets: list[StitchTarget] = []
    for entry in sorted(stitch_root.iterdir()):
        toml_path = entry / "stitch.toml"
        if not entry.is_dir() or not toml_path.is_file():
            continue
        version, explicit = _read_stitch_config(toml_path)
        prefixes = _infer_covered_prefixes(entry, entry.name, explicit)
        matched = tuple(
            path
            for path in changed_paths
            if any(path.startswith(prefix) for prefix in prefixes)
        )
        if not matched:
            continue
        target = StitchTarget(
            name=entry.name,
            directory=str(entry),
            stitch_version=version,
            matched_files=matched,
        )
        targets.append(target)
    return targets


def resolve_stitch_command(version: str | None, which: Which) -> list[str] | None:
    """Resolve the pinned `uvx --from stitch-fuzz==<ver> stitch` or `stitch` binary."""
    uvx = which("uvx")
    if uvx and version:
        return [uvx, "--from", f"stitch-fuzz=={version}", "stitch"]
    stitch_bin = which("stitch")
    if stitch_bin:
        return [stitch_bin]
    if uvx:
        return [uvx, "--from", "stitch-fuzz", "stitch"]
    return None


def _parse_crashes(output: str) -> list[dict[str, Any]]:
    text = output.strip()
    if not text:
        return []
    parsed = json.loads(text)
    if isinstance(parsed, list):
        return [item for item in parsed if isinstance(item, dict)]
    if isinstance(parsed, dict) and isinstance(parsed.get("crashes"), list):
        return [item for item in parsed["crashes"] if isinstance(item, dict)]
    return []


def _crash_id(crash: dict[str, Any]) -> str | None:
    for key in ("id", "crash_id", "name"):
        val = crash.get(key)
        if isinstance(val, str) and val.strip():
            return val.strip()
    return None


def _crash_summary(crash: dict[str, Any]) -> str:
    parts = [
        str(crash.get(key)).strip()
        for key in ("signal", "error", "message", "summary", "location")
        if crash.get(key)
    ]
    return "; ".join(parts) if parts else "crash reproduced by STITCH++"


def _attach_target(
    crash_text: str,
    matched_files: tuple[str, ...],
    added_by_path: dict[str, set[int]],
) -> tuple[str, int]:
    chosen = matched_files[0]
    for path in matched_files:
        if path in crash_text or os.path.basename(path) in crash_text:
            chosen = path
            break
    added_lines = added_by_path.get(chosen)
    line = min(added_lines) if added_lines else 1
    return chosen, line


def _base_also_crashes(
    cmd: list[str],
    crash_id: str,
    base_rev: str,
    target_dir: str,
    env: dict[str, str],
    executor: Executor,
) -> bool:
    argv = [*cmd, "exec", crash_id, "--rev", base_rev]
    code, out = executor(argv, target_dir, env, STITCH_CMD_TIMEOUT_S)
    lowered = out.lower()
    if "no crash" in lowered:
        return False
    return code != 0 or "crash" in lowered or "panic" in lowered


def _run_harness_target(
    stage: _Stage,
    target: StitchTarget,
    base_rev: str | None,
    probe_secs: int,
    which: Which,
    executor: Executor,
    env: dict[str, str],
    added_by_path: dict[str, set[int]],
) -> None:
    cmd = resolve_stitch_command(target.stitch_version, which)
    if cmd is None:
        stage.skip("neither uvx nor stitch is installed")
        return

    for subcmd in (["build"], ["check"], ["probe", "--secs", str(probe_secs)]):
        code, out = executor([*cmd, *subcmd], target.directory, env, STITCH_CMD_TIMEOUT_S)
        if code != 0:
            stage.status = "failed"
            stage.note = f"stitch/{target.name} {' '.join(subcmd)} failed: {_text(out, 140)}"
            return

    code, crashes_out = executor([*cmd, "crashes", "--json"], target.directory, env, STITCH_CMD_TIMEOUT_S)
    if code != 0:
        stage.status = "failed"
        stage.note = f"stitch/{target.name} crashes --json failed: {_text(crashes_out, 140)}"
        return

    try:
        crashes = _parse_crashes(crashes_out)
    except json.JSONDecodeError as error:
        stage.status = "failed"
        stage.note = f"stitch/{target.name} crashes --json returned invalid JSON: {error}"
        return
    for crash in crashes:
        if len(stage.findings) >= MAX_STAGE_FINDINGS:
            break
        cid = _crash_id(crash)
        if cid is None:
            continue
        if base_rev and _base_also_crashes(cmd, cid, base_rev, target.directory, env, executor):
            continue
        summary = _crash_summary(crash)
        path, line = _attach_target(summary, target.matched_files, added_by_path)
        proof = f" (no crash on base {base_rev[:8]})" if base_rev else ""
        claim = f"STITCH++ harness `stitch/{target.name}` reproduced crash `{cid}`{proof}"
        stage.findings.append(
            _finding(
                "stitch",
                path,
                line,
                claim,
                summary,
                level="critical",
                category="bugs",
                tool="stitch",
                host_confirmed=True,
            )
        )
        stage.flagged.add(path)

    stage.files.extend(target.matched_files)


def run_stitch_stage(
    stage: _Stage,
    repo_root: str,
    files: list[FileDiff],
    base_rev: str | None,
    which: Which,
    executor: Executor,
    env_fn: EnvFn,
) -> None:
    """Run matching `stitch/<target>/` harnesses and prove any crash against `base_rev`."""
    if not _stitch_enabled():
        stage.skip(f"disabled via {STITCH_ENV_FLAG}")
        return

    changed_paths = [item.path for item in files if item.status != "deleted" and not item.binary]
    targets = discover_matching_harnesses(repo_root, changed_paths)
    if not targets:
        stage.skip("no stitch/ harness covers the changed files")
        return

    if not which("docker"):
        stage.skip("docker is not installed")
        return

    env = dict(env_fn())
    env.setdefault("BUILDX_BUILDER", "default")
    added_by_path = {item.path: _new_lines(item) for item in files}
    probe_secs = _probe_seconds()

    for target in targets:
        _run_harness_target(stage, target, base_rev, probe_secs, which, executor, env, added_by_path)
        if stage.status in ("failed", "skipped"):
            return
