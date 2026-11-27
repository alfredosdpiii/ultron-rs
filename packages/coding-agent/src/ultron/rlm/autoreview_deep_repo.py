"""Shared repository wrapper, constants, contracts, and primitives for `autoreview_deep`."""
from __future__ import annotations

import posixpath
import re
from typing import Any

from review_api import Runner, _clip, _run_process, _text

GIT_TIMEOUT_S = 30
#: The only git subcommands the deep pass runs. All of them only read the object database.
ALLOWED_GIT = ("grep", "show", "ls-tree", "log")

DEFAULT_ROUNDS = 6
MAX_ROUNDS = 10
MAX_REQUESTS = 8
ROUND_CHARS = 48_000
#: A ledger line of an earlier result, re-readable by `recall`.
LEDGER_LINE_CHARS = 160
MAX_READ_LINES = 400
MAX_GREP_HITS = 50
DEFAULT_GREP_HITS = 20
#: Hits fetched for a scoped or count-only grep: its count is exact up to this many.
MAX_COUNT_HITS = 1_000
MAX_REFERENCES = 30
MAX_LIST = 200
MAX_PATTERN_CHARS = 200
MAX_FILE_BYTES = 2_000_000
BRIEF_CHARS = 8_000
#: Changed files whose likes the brief lists, and how many changed files it looks at for them.
MAX_LIKE = 6
MAX_LIKE_SCANNED = 12
DIFF_CHARS = 200_000
LEADS_CHARS = 4_000
#: The leads shown to the investigators; only these can be refuted.
MAX_LEADS = 20
LEADS_HEADER = (
    "Leads from the first pass (unverified; extend, correct or ignore them). "
    "A lead you checked and found false (the failure it names cannot happen) goes in refuted as "
    '{"lead": id, "why": "...", "evidence": [citations]}, '
    "not only in checked: the host drops it from the review.\n"
)
MAX_DEEP_FINDINGS = 8
MAX_CITATIONS = 6
CITATION_SLACK = 2
CITED_WINDOW = 18
DEDUPE_WINDOW = 3
MERGE_WINDOW = 5
LEVEL_ORDER = ("critical", "high", "medium", "low", "nit")
SEVERITY_BY_LEVEL = {
    "critical": "blocker",
    "high": "major",
    "medium": "minor",
    "low": "minor",
    "nit": "nit",
}
#: An investigator that is done in round 1 with fewer served lookups than this is sent back once.
MIN_LOOKUPS = 3
MAX_HISTORY = 20
MAX_BLAME_LINES = 60
#: Graph queries kept in an investigator's record.
MAX_RECORDED_QUERIES = 5
#: Review notes shown per request, and the characters of each.
MAX_NOTES = 8
NOTE_CHARS = 400

_CITATION = {
    "type": "object",
    "properties": {
        "path": {"type": "string"},
        "line": {"type": "integer"},
        "quote": {"type": "string"},
    },
    "required": ["path", "line", "quote"],
}

_SEVERITY_ENUM = [
    "critical",
    "high",
    "medium",
    "low",
    "nit",
    "blocker",
    "major",
    "minor",
]

_FINDING_PROPERTIES: dict[str, Any] = {
    "file": {"type": "string"},
    "line": {"type": "integer"},
    "severity": {"enum": _SEVERITY_ENUM},
    "category": {"type": "string"},
    "claim": {"type": "string"},
    "why": {"type": "string"},
    "scenario": {"type": "string"},
    "suggested_fix": {"type": "string"},
    "confidence": {"type": "number", "minimum": 0, "maximum": 1},
    "evidence": {"type": "array", "items": _CITATION},
    "unpinned": {"type": ["object", "null"]},
    "consequence": {"type": "string"},
    "rules": {"type": "array", "items": {"type": "string"}},
}

_FINDING_REQUIRED = [
    "file",
    "line",
    "severity",
    "category",
    "claim",
    "why",
    "scenario",
    "evidence",
]

_REFUTED_ITEM: dict[str, Any] = {
    "type": "object",
    "properties": {
        "lead": {"type": "integer"},
        "why": {"type": "string"},
        "evidence": {"type": "array", "items": _CITATION},
    },
    "required": ["lead", "why", "evidence"],
}

DEEP_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {
        "findings": {
            "type": "array",
            "maxItems": MAX_DEEP_FINDINGS,
            "items": {
                "type": "object",
                "properties": _FINDING_PROPERTIES,
                "required": _FINDING_REQUIRED,
            },
        },
        "requests": {"type": "array", "items": {"type": "object"}},
        "checked": {"type": "array", "items": {"type": "string"}},
        "refuted": {"type": "array", "items": _REFUTED_ITEM},
        "done": {"type": "boolean"},
    },
    "required": ["findings", "requests", "done"],
}

_TRIAGE_PATH_ITEM: dict[str, Any] = {
    "type": "object",
    "properties": {
        "id": {"type": "string"},
        "title": {"type": "string"},
        "question": {"type": "string"},
        "start_files": {"type": "array", "items": {"type": "string"}, "maxItems": 6},
        "focus": {"type": "string"},
    },
    "required": ["id", "title", "question", "start_files", "focus"],
}

TRIAGE_CONTRACT: dict[str, Any] = {
    "type": "object",
    "properties": {
        "paths": {
            "type": "array",
            "minItems": 1,
            "maxItems": 8,
            "items": _TRIAGE_PATH_ITEM,
        },
    },
    "required": ["paths"],
}

_SYMBOL = re.compile(r"^[A-Za-z_][A-Za-z0-9_.$-]{0,79}$")
_GLOB = re.compile(r"^[A-Za-z0-9_./*?\[\]{}, -]{1,160}$")

#: Non-code files whose literals are author choices rather than test-pinned code.
_NON_CODE = re.compile(
    r"\.(ya?ml|json|toml|ini|cfg|env|tf|tfvars|hcl|sql|sh|bash|zsh|"
    r"properties|xml|txt|csv|lock|md|mdx|rst|adoc)$"
    r"|(^|/)(Dockerfile|Makefile|\.env[^/]*)$",
    re.I,
)


class Rejected(Exception):
    """A request the host will not serve; the message goes back to the investigator."""


class Repo:
    """A repository at one commit, through a fixed set of read-only git subcommands."""

    def __init__(self, root: str, rev: str, runner: Runner | None = None) -> None:
        self.root = root
        self.rev = rev
        self._runner = runner or _run_process
        self._files: list[str] | None = None
        self._tracked: set[str] = set()
        self._cache: dict[str, list[str] | None] = {}
        #: Git calls made, for the record.
        self.calls = 0
        #: Structural reference of the change once the map builds it.
        self.reference: Any = None
        #: Knowledge store spec ({"db", "repo", "rev"}) when review notes are available.
        self.knowledge: dict[str, Any] | None = None
        #: Ids of rules `rules_for` served in this review; a finding may cite only these.
        self.served_rules: set[int] = set()
        #: Knowledge store spec ({"db", "repo", "rev"}) when the code graph is indexed.
        self.graph: dict[str, Any] | None = None
        #: Graph queries served in this review, for the record.
        self.graph_queries = 0
        #: Per-file diff sections indexed from the full diff so truncated files stay readable.
        self.diff_by_file: dict[str, str] = {}

    def _git(self, *args: str) -> tuple[int, str, str]:
        if not args or args[0] not in ALLOWED_GIT:
            raise AssertionError(f"the deep pass does not run git {args[:1]}")
        self.calls += 1
        return self._runner(["git", *args], self.root, GIT_TIMEOUT_S)

    def files(self) -> list[str]:
        if self._files is None:
            code, stdout, _ = self._git("ls-tree", "-r", "--name-only", "-z", self.rev)
            self._files = (
                [name for name in stdout.split("\0") if name] if code == 0 else []
            )
            self._tracked = set(self._files)
        return self._files

    def path(self, raw: Any) -> str:
        """`raw` as a tracked path of the commit, or Rejected."""
        if not isinstance(raw, str) or not raw.strip() or "\0" in raw or "\n" in raw:
            raise Rejected("path must be a string")
        text = raw.strip()
        if text.startswith(("/", "~", ":", "-")) or "\\" in text:
            raise Rejected(f"{text!r} is not a path inside the repository")
        if text.startswith("./"):
            text = text[2:]
        if ".." in text.split("/"):
            raise Rejected(f"{text!r} leaves the repository")
        normal = posixpath.normpath(text)
        self.files()
        if normal not in self._tracked:
            raise Rejected(f"{normal} is not a tracked file at the reviewed commit")
        return normal

    def lines(self, path: str) -> list[str] | None:
        """Lines of a tracked text file at the commit, or None if binary/too large."""
        if path not in self._cache:
            code, stdout, _ = self._git("show", f"{self.rev}:{path}")
            is_valid = (
                code == 0
                and "\0" not in stdout[:8192]
                and len(stdout) <= MAX_FILE_BYTES
            )
            self._cache[path] = stdout.splitlines() if is_valid else None
        return self._cache[path]

    def grep(
        self,
        pattern: str,
        *,
        fixed: bool = False,
        word: bool = False,
        pathspec: str | None = None,
        limit: int = DEFAULT_GREP_HITS,
    ) -> list[tuple[str, int, str]]:
        """(path, line, text) of matching lines in tracked text files, at most `limit`."""
        args = ["grep", "-n", "-I", "--no-color", "-F" if fixed else "-E"]
        if word:
            args.append("-w")
        args += ["-e", pattern, self.rev, "--", pathspec or "."]
        code, stdout, stderr = self._git(*args)
        if code not in (0, 1):
            raise Rejected(f"grep failed: {_text(stderr, 120)}")
        hits: list[tuple[str, int, str]] = []
        prefix = self.rev + ":"
        for raw in stdout.splitlines():
            if raw.startswith(prefix):
                raw = raw[len(prefix) :]
            path, _, rest = raw.partition(":")
            number, _, text = rest.partition(":")
            if number.isdigit():
                hits.append((path, int(number), text))
            if len(hits) >= limit:
                break
        return hits

    def history(self, path: str, count: int = 3) -> list[str]:
        """The last commits that touched a file: short hash, date, subject."""
        code, stdout, _ = self._git(
            "log",
            "-n",
            str(count),
            "--format=%h %ad %s",
            "--date=short",
            self.rev,
            "--",
            path,
        )
        return stdout.splitlines() if code == 0 else []

    def line_history(
        self, path: str, start: int, end: int, count: int = 5
    ) -> list[str]:
        """The commits that last changed lines start..end of a file."""
        code, stdout, _ = self._git(
            "log",
            f"-L{start},{end}:{path}",
            "-s",
            "-n",
            str(count),
            "--format=%h %ad %s",
            "--date=short",
            self.rev,
        )
        return [line for line in stdout.splitlines() if line.strip()] if code == 0 else []

    def pickaxe(self, text: str, count: int = 5) -> list[str]:
        """The commits that added or removed a string."""
        code, stdout, _ = self._git(
            "log",
            f"-S{text}",
            "-n",
            str(count),
            "--format=%h %ad %s",
            "--date=short",
            self.rev,
        )
        return stdout.splitlines() if code == 0 else []

    def messages(self, base: str | None, count: int = 8) -> list[str]:
        """Subjects and first body lines of the commits under review."""
        if not base:
            return []
        code, stdout, _ = self._git(
            "log",
            "-n",
            str(count),
            "--format=%s%n%b%x00",
            f"{base}..{self.rev}",
        )
        out: list[str] = []
        for entry in stdout.split("\0") if code == 0 else []:
            lines = [line.strip() for line in entry.strip().splitlines() if line.strip()]
            out += lines[:3]
        return out


def is_code(path: str) -> bool:
    """Whether a file holds testable code rather than config, manifests, scripts, or prose."""
    return not _NON_CODE.search(path)


def _numbered(lines: list[str], start: int, end: int) -> str:
    return "\n".join(
        f"{number:>5} | {_clip(lines[number - 1], 300)}"
        for number in range(start, end + 1)
    )


def _hits(hits: list[tuple[str, int, str]]) -> str:
    return (
        "\n".join(
            f"{path}:{line}: {_clip(text.strip(), 200)}" for path, line, text in hits
        )
        or "0 matches"
    )


def _definition_pattern(symbol: str) -> str:
    name = re.escape(symbol.rsplit(".", 1)[-1])
    keywords = "def|class|function|func|fn|interface|type|struct|enum|const|let|var"
    return (
        rf"(^|[^A-Za-z0-9_])({keywords})[ \t]+{name}([^A-Za-z0-9_]|$)"
        rf"|^[ \t]*{name}[ \t]*[:=][^=]"
    )


def _int(value: Any, default: int) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def _add(items: list[Any], item: Any, limit: int) -> None:
    if item not in items and len(items) < limit:
        items.append(item)


def _flat(text: str) -> str:
    return " ".join(text.split())


def _loose(text: str) -> str:
    cleaned = _flat(text)
    while "\\\\" in cleaned:
        cleaned = cleaned.replace("\\\\", "\\")
    return cleaned.replace("\\'", "'").replace('\\"', '"')


def _list(value: Any) -> list[Any]:
    return value if isinstance(value, list) else []
