#!/usr/bin/env python3
"""Exercise the Antigravity provider and the actual Ultron lane without an account or network model."""
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile

repo = Path(__file__).resolve().parents[1]
binary = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else repo / "target/debug/ultron"
with tempfile.TemporaryDirectory(prefix="ultron-agy-test-") as scratch:
    root = Path(scratch)
    value = secrets.token_hex(16)
    (root / "fixture.txt").write_text(value)
    env = {
        "HOME": scratch,
        "PATH": os.pathsep.join([str(repo / "crates/ai/fixtures/fake-agy"), "/usr/bin", "/bin"]),
        "ULTRON_SERVER_DIR": str(root / "server"),
        "ULTRON_CODING_AGENT_DIR": str(root / "agent"),
        "PI_OFFLINE": "1",
    }
    result = subprocess.run(
        [str(binary), "--offline", "--no-session", "--model", "antigravity-cli/offline-test", "-p", "Run the two-cell test."],
        cwd=root, env=env, capture_output=True, text=True, timeout=90,
    )
    if result.returncode or value[::-1] not in result.stdout:
        raise SystemExit(f"Same-lane test failed ({result.returncode}): {result.stdout[-1000:]} {result.stderr[-1000:]}")
    print("Antigravity tool calls shared the active Ultron kernel across two model responses.")
