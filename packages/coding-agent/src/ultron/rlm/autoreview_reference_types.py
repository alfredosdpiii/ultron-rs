"""Shared constants, patterns, and data structures for `autoreview_reference`."""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from typing import Any

#: Bounds of the index: files read, bytes parsed, call sites and families kept.
MAX_FILES = 200
MAX_BYTES = 6_000_000
MAX_IMPORTERS = 30
MAX_TEST_FILES = 20
MAX_CALLS_PER_FILE = 2_000
MAX_FAMILIES_PER_FILE = 60

#: Bounds of what is shown.
BLOCK_CHARS = 12_000
MAX_CALLERS_SHOWN = 8
MAX_TESTS_SHOWN = 5
MAX_BODY_LINES = 60

#: A literal family is indexed from two members; drift is reported from three.
MIN_FAMILY_ITEMS = 2
MIN_FAMILY_REPORTED = 3
MAX_LINE_CHARS = 160

_PY = re.compile(r"\.py$")
_TS = re.compile(r"\.(ts|tsx|js|jsx|mjs|cjs|mts|cts)$")
_WORKFLOW = re.compile(r"(^|/)\.github/workflows/[^/]+\.ya?ml$")
_TF = re.compile(r"\.tf$")
_TEST_PATH = re.compile(
    r"(^|/)(tests?|__tests__|spec|specs)/|[._-](test|spec)\.[^/]+$|(^|/)test_[^/]+\.py$",
    re.I,
)
_IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")

_TF_BLOCK = re.compile(
    r'^\s*(resource|data|module|variable|output|locals|provider|terraform)'
    r'(?:\s+"([^"]+)")?'
    r'(?:\s+"([^"]+)")?'
    r"\s*\{"
)


def _clip(text: str, limit: int = MAX_LINE_CHARS) -> str:
    text = text.strip()
    return text if len(text) <= limit else text[: limit - 1] + "…"


@dataclass
class Symbol:
    path: str
    name: str
    qualname: str
    kind: str  # function | method | class | constant | variable | interface | type | enum
    line: int
    end: int
    signature: str = ""
    parent: str | None = None
    bases: list[str] = field(default_factory=list)
    decorators: list[str] = field(default_factory=list)
    doc: str = ""
    exported: bool = True

    @property
    def key(self) -> str:
        return f"{self.path}::{self.qualname}"


@dataclass
class CallSite:
    path: str
    line: int
    callee: str
    text: str
    enclosing: str | None
    #: Keys of the definitions the call resolves to (empty: unresolved, matched by name only).
    targets: list[str] = field(default_factory=list)


@dataclass
class Family:
    """A literal list, tuple, set or array of strings: registries and member lists changes drift on."""

    path: str
    line: int
    name: str | None
    items: tuple[str, ...]
    enclosing: str | None = None


@dataclass
class FileIndex:
    path: str
    lang: str
    symbols: list[Symbol] = field(default_factory=list)
    #: Local name -> (module path in the repository or None, imported name or "*" for a module).
    imports: dict[str, tuple[str | None, str]] = field(default_factory=dict)
    calls: list[CallSite] = field(default_factory=list)
    families: list[Family] = field(default_factory=list)
    #: Names the file exports (TS/JS), or every top-level name (Python).
    exports: set[str] = field(default_factory=set)
    parsed: bool = True
    checked: bool = False


class YamlNode:
    """A mapping or sequence read from YAML, with the line of each entry."""

    __slots__ = ("value", "line", "lines")

    def __init__(self, value: Any, line: int) -> None:
        self.value = value
        self.line = line
        #: For a mapping: key -> line; for a sequence: index -> line.
        self.lines: dict[Any, int] = {}


@dataclass
class TfBlock:
    kind: str  # resource | data | module | variable | output | locals | provider | terraform
    type: str
    name: str
    path: str
    line: int
    end: int
    attrs: dict[str, str] = field(default_factory=dict)
    nested: dict[str, dict[str, str]] = field(default_factory=dict)
    refs: set[str] = field(default_factory=set)

    @property
    def address(self) -> str:
        if self.kind == "resource":
            return f"{self.type}.{self.name}"
        if self.kind == "data":
            return f"data.{self.type}.{self.name}"
        return f"{self.kind}.{self.type}" if self.type else self.kind
