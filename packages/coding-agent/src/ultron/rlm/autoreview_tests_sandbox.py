"""Sandboxed process execution and isolation primitives for `autoreview_tests`."""
from __future__ import annotations

import contextlib
import json
import os
import shutil
import signal
import subprocess
import tempfile
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

DEFAULT_RUNS = 6
DEFAULT_TIMEOUT_S = 300
MAX_OUTPUT_BYTES = 400_000
OUTPUT_TAIL_CHARS = 2_000
MAX_PATHS = 12
MAX_SELECT_CHARS = 120
MAX_REPLACEMENT_CHARS = 300
SANDBOX_HOME = "/tmp/home"
SANDBOX_PATH = "/usr/local/bin:/usr/bin:/bin"
PYTHON_ENVS = (".venv", "venv")
NODE_ENV = "node_modules"

#: (argv, cwd, env, timeout seconds) -> (exit code, combined output); 124 timeout, 127 missing.
Executor = Callable[[list[str], str, dict[str, str], float], "tuple[int, str]"]
Which = Callable[[str], "str | None"]


class TestsRejected(Exception):
    """A test request the host will not run; the message goes back to the investigator."""


_SYSTEM_DIRS = ("/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32")
_ETC = (
    "ld.so.cache",
    "ld.so.conf",
    "ld.so.conf.d",
    "passwd",
    "group",
    "nsswitch.conf",
    "localtime",
    "alternatives",
    "ssl",
    "ca-certificates",
    "mime.types",
    "os-release",
)
_HIDDEN = ("/home", "/root", "/run", "/media", "/srv", "/var/run", "/var/lib/docker")

_UNSHARE_SCRIPT = """set -e
work="$1"; shift
mount --bind "$work" /mnt
mount -t tmpfs tmpfs /tmp
while [ "$1" != "--" ]; do if [ -d "$1" ]; then mount -t tmpfs tmpfs "$1"; fi; shift; done
shift
mkdir -p "$work" /tmp/home
mount --move /mnt "$work" 2>/dev/null || mount --bind /mnt "$work"
cd "$work"
exec "$@"
"""

_DOCKER_CLIENT = 'exec "$@" < <(exec setpriv --pdeathsig KILL -- sleep infinity)'
_DOCKER_CLIENT_POLLING = 'exec "$@" < <(while kill -0 $$ 2>/dev/null; do sleep 1; done)'
_DOCKER_TIED = (
    'exec 3<&0; "$@" </dev/null 3<&- & c=$!; '
    "{ cat <&3 >/dev/null; kill -KILL $c 2>/dev/null; } & "
    "w=$!; exec 3<&-; wait $c; s=$?; kill $w 2>/dev/null; exit $s"
)

_SELF_CHECK = r"""
import os, socket, sys
out = {}
try:
    socket.create_connection(("1.1.1.1", 53), 2).close()
    out["network"] = "reachable"
except OSError:
    out["network"] = "unreachable"
out["canary"] = "readable" if os.path.exists(sys.argv[1]) else "unreadable"
out["token"] = "present" if any("CANARY_TOKEN" in name for name in os.environ) else "absent"
leaked = [
    name for name in os.environ
    if any(
        word in name.upper()
        for word in ("TOKEN", "SECRET", "KEY", "PASSWORD", "AWS_", "SSH_", "GH_", "GITHUB")
    )
]
out["credentialVariables"] = leaked
out["home"] = os.environ.get("HOME")
try:
    open("probe.txt", "w").write("x")
    out["workdir"] = "writable"
except OSError:
    out["workdir"] = "read-only"
try:
    open("/usr/ultron-autoreview-probe", "w").write("x")
    out["system"] = "writable"
except OSError:
    out["system"] = "read-only"
has_sock = os.path.exists("/var/run/docker.sock") or os.path.exists("/run/docker.sock")
out["dockerSocket"] = "visible" if has_sock else "hidden"
import json; print("SELFCHECK " + json.dumps(out))
"""

_SELF_CHECK_EXPECTED = {
    "network": "unreachable",
    "canary": "unreadable",
    "token": "absent",
    "workdir": "writable",
    "system": "read-only",
    "dockerSocket": "hidden",
    "home": SANDBOX_HOME,
}


def sandbox_env(extra_path: str | None = None) -> dict[str, str]:
    """Build a clean sandbox environment from scratch with no caller credentials."""
    prefix = f"{extra_path}:" if extra_path else ""
    return {
        "PATH": prefix + SANDBOX_PATH,
        "HOME": SANDBOX_HOME,
        "TMPDIR": "/tmp",
        "LANG": "C.UTF-8",
        "TERM": "dumb",
        "NO_COLOR": "1",
        "CI": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
        "PYTHONHASHSEED": "0",
    }


def _normalize_binds(
    ro_binds: "list[str | tuple[str, str]] | None",
) -> list[tuple[str, str]]:
    if not ro_binds:
        return []
    binds: list[tuple[str, str]] = []
    for item in ro_binds:
        if not item:
            continue
        binds.append((item, item) if isinstance(item, str) else item)
    return binds


def _wrap_bwrap(
    command: list[str],
    workdir: str,
    inside: str,
    env: dict[str, str],
    binds: list[tuple[str, str]],
) -> list[str]:
    argv = [
        "bwrap",
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--cap-drop",
        "ALL",
        "--clearenv",
    ]
    for name, value in env.items():
        argv += ["--setenv", name, value]
    for path in _SYSTEM_DIRS:
        if os.path.islink(path):
            argv += ["--symlink", os.readlink(path), path]
        elif os.path.isdir(path):
            argv += ["--ro-bind", path, path]
    for name in _ETC:
        argv += ["--ro-bind-try", f"/etc/{name}", f"/etc/{name}"]

    argv += ["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--dir", SANDBOX_HOME]
    argv += ["--bind", workdir, workdir]
    for source, target in binds:
        argv += ["--ro-bind", source, target]
    return [*argv, "--chdir", inside, "--", *command]


def _wrap_unshare(
    command: list[str],
    workdir: str,
    env: dict[str, str],
    home: str,
) -> list[str]:
    hidden = list(dict.fromkeys([*_HIDDEN, home]))
    env_args = [f"{name}={value}" for name, value in env.items()]
    return [
        "unshare",
        "--user",
        "--map-root-user",
        "--mount",
        "--net",
        "--pid",
        "--fork",
        "--ipc",
        "--uts",
        "--kill-child",
        "env",
        "-i",
        *env_args,
        "sh",
        "-c",
        _UNSHARE_SCRIPT,
        "sandbox",
        workdir,
        *hidden,
        "--",
        *command,
    ]


def _wrap_docker(
    command: list[str],
    workdir: str,
    env: dict[str, str],
    binds: list[tuple[str, str]],
    image: str | None,
) -> list[str]:
    # Tie container lifetime to client stdin so killing the client ends the container.
    setpriv = shutil.which("setpriv") is not None
    argv = ["setpriv", "--pdeathsig", "KILL", "--"] if setpriv else []
    client_script = _DOCKER_CLIENT if setpriv else _DOCKER_CLIENT_POLLING
    argv += [
        "bash",
        "-c",
        client_script,
        "ultron-docker",
        "docker",
        "run",
        "--rm",
        "--init",
        "-i",
        "--network",
        "none",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--pids-limit",
        "1024",
        "--user",
        f"{os.getuid()}:{os.getgid()}",
        "--tmpfs",
        "/tmp",
        "-v",
        f"{workdir}:{workdir}:rw",
        "-w",
        workdir,
    ]
    for source, target in binds:
        argv += ["-v", f"{source}:{target}:ro"]

    env_args = [f"{name}={value}" for name, value in env.items()]
    return [*argv, str(image), "sh", "-c", _DOCKER_TIED, "sh", "env", "-i", *env_args, *command]


@dataclass
class Sandbox:
    """How commands are isolated: `mechanism` is "bwrap", "unshare" or "docker"."""

    mechanism: str
    image: str | None = None
    home: str = field(default_factory=lambda: os.path.expanduser("~"))

    def wrap(
        self,
        command: list[str],
        workdir: str,
        env: dict[str, str],
        ro_binds: "list[str | tuple[str, str]] | None" = None,
        cwd: str = "",
    ) -> list[str]:
        """Build the full sandboxed command line running in `workdir` or `cwd`."""
        binds = _normalize_binds(ro_binds)
        inside = os.path.join(workdir, cwd) if cwd else workdir
        if cwd and self.mechanism != "bwrap":
            command = ["sh", "-c", 'cd "$1" && shift && exec "$@"', "cd", inside, *command]

        if self.mechanism == "bwrap":
            return _wrap_bwrap(command, workdir, inside, env, binds)
        if self.mechanism == "unshare":
            return _wrap_unshare(command, workdir, env, self.home)
        if self.mechanism == "docker":
            return _wrap_docker(command, workdir, env, binds, self.image)
        raise ValueError(f"unknown sandbox mechanism {self.mechanism}")

    def describe(self) -> str:
        descriptions = {
            "bwrap": (
                "bubblewrap: new user, mount, pid, ipc, uts, cgroup and network namespaces; "
                "an empty root with the system directories read-only, a private /tmp and HOME, "
                "and the exported commit as the only writable directory; all capabilities "
                "dropped; environment cleared"
            ),
            "unshare": (
                "unshare: new user, mount, pid, ipc, uts and network namespaces; /home, /root, "
                "/run and the user's home hidden under empty tmpfs mounts, a private /tmp; "
                "environment cleared"
            ),
            "docker": (
                f"docker ({self.image}): no network, all capabilities dropped, no new "
                "privileges, only the exported commit mounted; environment cleared; the "
                "container ends with its client"
            ),
        }
        return descriptions[self.mechanism]


def run_process(
    argv: list[str],
    cwd: str,
    env: dict[str, str],
    timeout: float,
) -> tuple[int, str]:
    """Run to completion in its own process group; kill the group on timeout; cap output."""
    try:
        process = subprocess.Popen(
            argv,
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    except FileNotFoundError:
        return 127, f"{argv[0]}: not found"

    try:
        output, _ = process.communicate(timeout=timeout)
        code = process.returncode
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except OSError:
            process.kill()
        output, _ = process.communicate()
        code = 124

    text = output[-MAX_OUTPUT_BYTES:].decode("utf-8", "replace")
    return code, text


def _launch_env() -> dict[str, str]:
    """Environment of the sandbox launcher itself: PATH and optional DOCKER_HOST."""
    env = {"PATH": os.environ.get("PATH", SANDBOX_PATH)}
    if "DOCKER_HOST" in os.environ:
        env["DOCKER_HOST"] = os.environ["DOCKER_HOST"]
    return env


def _works(sandbox: Sandbox, executor: Executor) -> bool:
    probe = tempfile.mkdtemp(prefix="ultron-autoreview-probe-")
    try:
        code, _ = executor(sandbox.wrap(["true"], probe, sandbox_env()), probe, _launch_env(), 30)
        return code == 0
    finally:
        shutil.rmtree(probe, ignore_errors=True)


def detect_sandbox(
    *,
    image: str | None = None,
    which: Which | None = None,
    executor: Executor = run_process,
    only: str | None = None,
) -> Sandbox | None:
    """Return the strongest working sandbox (bwrap, unshare, or docker), or None."""
    which_fn = which if which is not None else shutil.which
    for mechanism in ("bwrap", "unshare", "docker"):
        if only is not None and mechanism != only:
            continue
        if which_fn(mechanism) is None or (mechanism == "docker" and not image):
            continue

        sandbox = Sandbox(mechanism, image if mechanism == "docker" else None)
        if _works(sandbox, executor):
            return sandbox
    return None


def _parse_self_check_output(
    code: int,
    output: str,
) -> tuple[dict[str, Any] | None, dict[str, Any] | None]:
    line = next((item for item in output.splitlines() if item.startswith("SELFCHECK ")), None)
    if line is None:
        return None, {
            "ok": False,
            "error": f"the self-check did not run (exit {code}): {output[-300:]}",
        }
    return json.loads(line[len("SELFCHECK ") :]), None


def _self_check_passed(result: dict[str, Any]) -> bool:
    for key, target in _SELF_CHECK_EXPECTED.items():
        if result.get(key) != target:
            return False
    return not result.get("credentialVariables")


def self_check(sandbox: Sandbox, executor: Executor = run_process) -> dict[str, Any]:
    """Run isolation checks inside the sandbox."""
    home = os.path.expanduser("~")
    canary = os.path.join(home, f".ultron-autoreview-canary-{uuid.uuid4().hex[:8]}")
    workdir = tempfile.mkdtemp(prefix="ultron-autoreview-check-")
    launch = {**_launch_env(), "ULTRON_AUTOREVIEW_CANARY_TOKEN": "not-a-real-token"}
    try:
        Path(canary).write_text("canary\n", encoding="utf-8")
        argv = sandbox.wrap(["python3", "-c", _SELF_CHECK, canary], workdir, sandbox_env())
        code, output = executor(argv, workdir, launch, 60)
        result, error_payload = _parse_self_check_output(code, output)
        if error_payload is not None or result is None:
            return error_payload if error_payload is not None else {"ok": False}

        result["ok"] = _self_check_passed(result)
        return result
    finally:
        with contextlib.suppress(OSError):
            os.unlink(canary)
        shutil.rmtree(workdir, ignore_errors=True)


def doctor(image: str | None = None) -> dict[str, Any]:
    sandbox = detect_sandbox(image=image)
    if sandbox is None:
        return {
            "mechanism": None,
            "ok": False,
            "message": (
                "tests are not run: no sandbox is available (install bubblewrap, or allow "
                "unprivileged user namespaces, or set autoreview.testImage for Docker)"
            ),
        }

    check = self_check(sandbox)
    return {
        "mechanism": sandbox.mechanism,
        "isolation": sandbox.describe(),
        "selfCheck": check,
        "ok": bool(check.get("ok")),
    }


def export_commit(root: str, rev: str) -> str:
    """Create a disposable copy of the commit's tree via `git archive`."""
    target = tempfile.mkdtemp(prefix="ultron-autoreview-run-")
    env = {
        "PATH": os.environ.get("PATH", SANDBOX_PATH),
        "GIT_OPTIONAL_LOCKS": "0",
        "GIT_TERMINAL_PROMPT": "0",
    }
    archive = subprocess.Popen(
        ["git", "-C", root, "archive", "--format=tar", rev],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        stdin=subprocess.DEVNULL,
        env=env,
    )
    extract = subprocess.run(
        ["tar", "-x", "-C", target],
        stdin=archive.stdout,
        capture_output=True,
        env=env,
    )
    if archive.stdout is not None:
        archive.stdout.close()
    if archive.wait() != 0 or extract.returncode != 0:
        shutil.rmtree(target, ignore_errors=True)
        raise RuntimeError(f"the commit {rev[:12]} could not be exported")
    return target
