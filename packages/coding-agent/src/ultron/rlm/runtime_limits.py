"""Process resource limits (RLIMIT_DATA, RLIMIT_CPU, core dumps, subreaper) for `runtime`."""
from __future__ import annotations

import contextlib
import importlib
import importlib.util
import math
import os
import signal
import sys
from typing import Any, Callable


def _env_limit(name: str) -> int:
    try:
        return max(0, int(os.environ.get(name, "0")))
    except ValueError:
        return 0


_MAX_MEMORY_MB = _env_limit("ULTRON_RLM_MAX_MEMORY_MB")
_MAX_CPU_SECONDS = _env_limit("ULTRON_RLM_MAX_CPU_SECONDS")
_EXIT_MEMORY = 86
_EXIT_CPU = 87
_EXIT_INTERNAL = 70


class RlmCpuLimitExceeded(BaseException):
    """Raised in the cell when it uses more than ULTRON_RLM_MAX_CPU_SECONDS of CPU time."""


def _cpu_used() -> float:
    import resource

    usage = resource.getrusage(resource.RUSAGE_SELF)
    return usage.ru_utime + usage.ru_stime


def _arm_cpu_limit() -> None:
    """Give the process `_MAX_CPU_SECONDS` more CPU before SIGXCPU; children inherit the soft limit."""
    if _MAX_CPU_SECONDS <= 0 or importlib.util.find_spec("resource") is None:
        return
    resource = importlib.import_module("resource")
    with contextlib.suppress(OSError, ValueError):
        _soft, hard = resource.getrlimit(resource.RLIMIT_CPU)
        soft = math.ceil(_cpu_used()) + _MAX_CPU_SECONDS
        if hard != resource.RLIM_INFINITY:
            soft = min(soft, hard)
        resource.setrlimit(resource.RLIMIT_CPU, (soft, hard))


def _disable_core_dumps(resource_mod: Any) -> None:
    if hasattr(resource_mod, "RLIMIT_CORE"):
        with contextlib.suppress(ValueError, OSError):
            resource_mod.setrlimit(resource_mod.RLIMIT_CORE, (0, 0))


def _apply_memory_limit(resource_mod: Any) -> None:
    if _MAX_MEMORY_MB <= 0 or not hasattr(resource_mod, "RLIMIT_DATA") or sys.platform == "darwin":
        return
    limit = _MAX_MEMORY_MB * 1024 * 1024
    with contextlib.suppress(ValueError, OSError):
        _soft, hard = resource_mod.getrlimit(resource_mod.RLIMIT_DATA)
        if hard != resource_mod.RLIM_INFINITY:
            limit = min(limit, hard)
        resource_mod.setrlimit(resource_mod.RLIMIT_DATA, (limit, limit))


def apply_resource_limits(on_cpu_limit: Callable[[int, Any], None]) -> None:
    """Apply core-dump, memory, and CPU limits for the kernel process."""
    if importlib.util.find_spec("resource") is None:
        return
    resource = importlib.import_module("resource")
    _disable_core_dumps(resource)
    _apply_memory_limit(resource)
    if _MAX_CPU_SECONDS > 0 and hasattr(signal, "SIGXCPU"):
        signal.signal(signal.SIGXCPU, on_cpu_limit)
        _arm_cpu_limit()


def _become_subreaper() -> None:
    """Adopt orphaned descendants on Linux so the host memory watchdog tracks them."""
    if not sys.platform.startswith("linux"):
        return
    import ctypes

    with contextlib.suppress(OSError, AttributeError):
        libc = ctypes.CDLL(None, use_errno=True)
        libc.prctl(36, 1, 0, 0, 0)


def _memory_limit_note() -> str:
    return (
        f"RLM kernel exceeded its memory limit ({_MAX_MEMORY_MB} MiB per process, "
        "ULTRON_RLM_MAX_MEMORY_MB); the allocation failed and the kernel keeps running"
    )
