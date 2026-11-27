"""Running the reviewed project's tests for `ultron autoreview`, sandboxed."""
from __future__ import annotations

import json
import sys

from autoreview_tests_env import (
    discover_environment as discover_environment,
    interpreter_home as interpreter_home,
    list_environments as list_environments,
)
from autoreview_tests_runners import (
    TestRunner as TestRunner,
    _package_runner as _package_runner,
    detect_runner as detect_runner,
    parse_outcome as parse_outcome,
    runner_for as runner_for,
)
from autoreview_tests_sandbox import (
    DEFAULT_RUNS as DEFAULT_RUNS,
    DEFAULT_TIMEOUT_S as DEFAULT_TIMEOUT_S,
    MAX_OUTPUT_BYTES as MAX_OUTPUT_BYTES,
    MAX_PATHS as MAX_PATHS,
    MAX_REPLACEMENT_CHARS as MAX_REPLACEMENT_CHARS,
    MAX_SELECT_CHARS as MAX_SELECT_CHARS,
    NODE_ENV as NODE_ENV,
    OUTPUT_TAIL_CHARS as OUTPUT_TAIL_CHARS,
    PYTHON_ENVS as PYTHON_ENVS,
    SANDBOX_HOME as SANDBOX_HOME,
    SANDBOX_PATH as SANDBOX_PATH,
)
from autoreview_tests_sandbox import (
    Executor as Executor,
    Sandbox as Sandbox,
    TestsRejected as TestsRejected,
    _launch_env as _launch_env,
    detect_sandbox as detect_sandbox,
    doctor as doctor,
    export_commit as export_commit,
    run_process as run_process,
    sandbox_env as sandbox_env,
    self_check as self_check,
)
from autoreview_tests_session import (
    TestSession as TestSession,
    summarize as summarize,
)

if __name__ == "__main__":
    if sys.argv[1:2] == ["doctor"]:
        print(json.dumps(doctor(sys.argv[2] if len(sys.argv) > 2 else None)))
    elif sys.argv[1:2] == ["environments"] and len(sys.argv) == 3:
        print(json.dumps(list_environments(sys.argv[2])))
    else:
        print("usage: autoreview_tests.py doctor [docker-image]", file=sys.stderr)
        sys.exit(2)
