"""The secret rules (``secret-patterns.json`` beside this file) belong to the host, which masks cell output with
them; the runtime only needs the marker the host leaves in place of a secret."""

from __future__ import annotations

import re
from pathlib import Path

PATTERNS_PATH = Path(__file__).with_name("secret-patterns.json")

# What the host puts in cell output in place of a secret.
REDACTION_MARKER = re.compile(r"\[REDACTED:[A-Za-z0-9_]+\]")
