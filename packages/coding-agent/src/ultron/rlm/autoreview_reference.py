"""Structural reference of the code a change touches, built before any model runs.

The deep pass's map found names with regexes and looked them up with `git grep`.
This module reads the files themselves:

- Python through `ast`: definitions with line ranges and signatures, imports
  resolved to repository files, call sites resolved to definitions, class
  hierarchies (overrides), test functions, and literal string families;
- TypeScript and JavaScript via a trusted PATH compiler when available (or a
  structural scanner fallback): definitions, imports, exports, call sites,
  and literal string arrays;
- GitHub Actions workflows via a small YAML reader, and Terraform via a block
  scanner, for permissions, triggers, guards, interpolations, and references.

Only what the change can reach is indexed, bounded in files and bytes: changed
files, their importers, imported modules, and tests naming changed symbols.
The model gets a bounded view with ids (`block()`), exact lookups (`symbol`,
`callers`, `callees`, `tests_of`), and host-observed facts (`facts()`,
`structural_findings()`). Facts never post themselves: structural findings go
through the verifier like any other, with host-checked citations.

Dependency-free, standard library only. Every reader is defensive: a file that
does not parse is skipped, and a reader failure leaves other results standing.
"""
from __future__ import annotations

import os
import posixpath
import re
import time
from typing import Any, Callable, Iterable

from autoreview_reference_facts import (
    _OIDC_RUN as _OIDC_RUN,
    _OIDC_USERS as _OIDC_USERS,
    _SAME_REPO_GUARD as _SAME_REPO_GUARD,
    _USER_CONTROLLED as _USER_CONTROLLED,
    _permissions as _permissions,
    _terraform_facts as _terraform_facts,
    _workflow_facts as _workflow_facts,
)
from autoreview_reference_parsers import (
    _TF_ATTR as _TF_ATTR,
    _TF_NESTED as _TF_NESTED,
    _TF_REF as _TF_REF,
    _TS_ARRAY as _TS_ARRAY,
    _TS_ARROW as _TS_ARROW,
    _TS_CHECKER as _TS_CHECKER,
    _TS_CLASS as _TS_CLASS,
    _TS_CONST as _TS_CONST,
    _TS_EXPORT as _TS_EXPORT,
    _TS_FUNC as _TS_FUNC,
    _TS_IMPORT as _TS_IMPORT,
    _TS_KEYWORDS as _TS_KEYWORDS,
    _TS_METHOD as _TS_METHOD,
    _TS_REQUIRE as _TS_REQUIRE,
    _TS_TYPE as _TS_TYPE,
    _PyVisitor as _PyVisitor,
    _py_family as _py_family,
    _py_name as _py_name,
    _py_signature as _py_signature,
    _split_flow as _split_flow,
    _strip_comment as _strip_comment,
    _ts_block_end as _ts_block_end,
    _ts_tools as _ts_tools,
    _yaml_get as _yaml_get,
    _yaml_scalar as _yaml_scalar,
    index_python as index_python,
    index_ts as index_ts,
    index_ts_checked as index_ts_checked,
    parse_terraform as parse_terraform,
    parse_yaml as parse_yaml,
)
from autoreview_reference_types import (
    BLOCK_CHARS as BLOCK_CHARS,
    MAX_BODY_LINES as MAX_BODY_LINES,
    MAX_BYTES as MAX_BYTES,
    MAX_CALLERS_SHOWN as MAX_CALLERS_SHOWN,
    MAX_CALLS_PER_FILE as MAX_CALLS_PER_FILE,
    MAX_FAMILIES_PER_FILE as MAX_FAMILIES_PER_FILE,
    MAX_FILES as MAX_FILES,
    MAX_IMPORTERS as MAX_IMPORTERS,
    MAX_LINE_CHARS as MAX_LINE_CHARS,
    MAX_TEST_FILES as MAX_TEST_FILES,
    MAX_TESTS_SHOWN as MAX_TESTS_SHOWN,
    MIN_FAMILY_ITEMS as MIN_FAMILY_ITEMS,
    MIN_FAMILY_REPORTED as MIN_FAMILY_REPORTED,
    CallSite as CallSite,
    Family as Family,
    FileIndex as FileIndex,
    Symbol as Symbol,
    TfBlock as TfBlock,
    YamlNode as YamlNode,
    _IDENT as _IDENT,
    _PY as _PY,
    _TEST_PATH as _TEST_PATH,
    _TF as _TF,
    _TF_BLOCK as _TF_BLOCK,
    _TS as _TS,
    _WORKFLOW as _WORKFLOW,
    _clip as _clip,
)

_IMPORT_LINE = re.compile(
    r"^\s*(?:from\s+([\w.]+)\s+import|import\s+([\w.]+))"
    r"|require\(\s*['\"]([^'\"]+)['\"]\s*\)"
    r"|from\s+['\"]([^'\"]+)['\"]"
)
_TS_JS_EXT_MAP = {".mjs": ".mts", ".cjs": ".cts"}
_TS_EXTENSIONS = (
    "",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    ".mjs",
    "/index.ts",
    "/index.tsx",
    "/index.js",
)


def _is_code_path(path: str) -> bool:
    return bool(_PY.search(path) or _TS.search(path))


def _module_stem(path: str) -> str | None:
    if not _is_code_path(path):
        return None
    stem = posixpath.splitext(posixpath.basename(path))[0]
    if stem in ("__init__", "index"):
        stem = posixpath.basename(posixpath.dirname(path))
    if not stem or not _IDENT.match(stem.replace("-", "_")):
        return None
    return stem


def _format_family_diff(missing: list[str], extra: list[str]) -> str:
    if not missing and not extra:
        return "same"
    parts: list[str] = []
    if missing:
        parts.append("missing " + ", ".join(repr(item) for item in missing[:4]))
    if extra:
        parts.append("extra " + ", ".join(repr(item) for item in extra[:4]))
    return "; ".join(parts)


def _compare_family(family: Family, mine: set[str], other: Family) -> tuple[Family, str] | None:
    if other.path == family.path and other.line == family.line:
        return None
    theirs = set(other.items)
    shared = mine & theirs
    if len(shared) < MIN_FAMILY_ITEMS or len(shared) < 0.6 * min(len(mine), len(theirs)):
        return None
    return (other, _format_family_diff(sorted(mine - theirs), sorted(theirs - mine)))


def _format_callers_line(callers: list[CallSite]) -> str:
    if not callers:
        return "  callers outside the change: none found in the indexed files"
    shown = callers[:MAX_CALLERS_SHOWN]
    entries = [
        f"{call.path}:{call.line}"
        + (f" in {call.enclosing}" if call.enclosing else "")
        + f": {_clip(call.text, 90)}"
        for call in shown
    ]
    suffix = f"; ... {len(callers) - len(shown)} more" if len(callers) > len(shown) else ""
    return f"  callers outside the change ({len(callers)}): " + "; ".join(entries) + suffix


class Reference:
    """The index and its views. `build` reads what the change can reach; queries
    never touch the repository except via the `Repo` passed in."""

    def __init__(
        self,
        repo: Any,
        files: list[Any],
        *,
        base_repo: Any = None,
        clock: Callable[[], float] = time.monotonic,
        max_files: int = MAX_FILES,
        max_bytes: int = MAX_BYTES,
    ) -> None:
        self.repo = repo
        self.base_repo = base_repo
        self.files = [item for item in files if not getattr(item, "binary", False)]
        self.clock = clock
        self.max_files = max_files
        self.max_bytes = max_bytes

        self.index: dict[str, FileIndex] = {}
        self.workflows: dict[str, YamlNode] = {}
        self.terraform: dict[str, list[TfBlock]] = {}
        self.changed_lines: dict[str, set[int]] = {}
        self.removed_text: dict[str, list[str]] = {}
        self.base_index: dict[str, FileIndex] = {}
        self._ts_sources: dict[str, str] = {}

        self.stats: dict[str, Any] = {
            "files": 0,
            "bytes": 0,
            "symbols": 0,
            "calls": 0,
            "families": 0,
            "workflows": 0,
            "terraform": 0,
            "ms": 0,
            "skipped": 0,
            "tsChecker": False,
        }
        self._modules: dict[str, str] = {}
        self._ids: dict[str, str] = {}
        self._facts: list[dict[str, Any]] | None = None

    def _repo_lines(self, path: str) -> list[str]:
        lines = self.repo.lines(path)
        return lines if lines is not None else []

    def _record_diff_metadata(self) -> None:
        for item in self.files:
            self.changed_lines[item.path] = {
                line.new
                for hunk in item.hunks
                for line in hunk.lines
                if line.kind == "+" and line.new
            }
            self.removed_text[item.path] = [
                line.text
                for hunk in item.hunks
                for line in hunk.lines
                if line.kind == "-"
            ]

    def _terraform_siblings(self, tracked: set[str], wanted: list[str]) -> list[str]:
        siblings: list[str] = []
        for path in list(self.changed_lines):
            if not _TF.search(path):
                continue
            directory = posixpath.dirname(path)
            found = [
                other
                for other in sorted(tracked)
                if _TF.search(other)
                and posixpath.dirname(other) == directory
                and other not in wanted
                and other not in siblings
            ]
            siblings.extend(found[:20])
        return siblings

    def _extend_unique(self, target: list[str], candidates: Iterable[str]) -> None:
        for item in candidates:
            if item not in target:
                target.append(item)

    def _collect_wanted_paths(self, changed: list[str], tracked: set[str]) -> list[str]:
        wanted: list[str] = list(changed)
        wanted.extend(self._importers(changed, tracked))
        for path in changed:
            self._read(path)
        for path in list(wanted):
            if path in self.index:
                self._extend_unique(wanted, self._imported_paths(self.index[path]))
        wanted.extend(self._test_files(changed, tracked))
        # Terraform references cross files of one directory: siblings of a changed .tf file are read too.
        wanted.extend(self._terraform_siblings(tracked, wanted))
        return wanted

    def _at_budget(self) -> bool:
        total_files = len(self.index) + len(self.workflows) + len(self.terraform)
        return total_files >= self.max_files or self.stats["bytes"] >= self.max_bytes

    def _read_wanted_paths(self, wanted: list[str]) -> None:
        for path in wanted:
            if self._at_budget():
                self.stats["skipped"] += 1
                continue
            self._read(path)
            if path in self._ts_sources:
                self._extend_unique(wanted, self._imported_paths(self.index[path]))

    def _resolve_index_imports(self, index: FileIndex) -> None:
        for local, (module_path, name) in list(index.imports.items()):
            if module_path is not None:
                continue
            resolved = self._resolve_module(name, index.path)
            index.imports[local] = (resolved, name.split(":")[-1])

    def _resolve_all_imports_and_calls(self) -> None:
        for index in self.index.values():
            self._resolve_index_imports(index)
        for index in self.index.values():
            self._resolve_calls(index)

    def _index_base_files(self, changed: list[str]) -> None:
        if self.base_repo is None:
            return
        for path in changed:
            if not _is_code_path(path):
                continue
            lines = self.base_repo.lines(path)
            if lines:
                indexer = index_python if _PY.search(path) else index_ts
                self.base_index[path] = indexer(path, "\n".join(lines))

    def _collect_base_ts_sources(self) -> dict[str, str]:
        if self.base_repo is None:
            return {}
        base_sources: dict[str, str] = {}
        for path in self._ts_sources:
            lines = self.base_repo.lines(path)
            if lines is not None:
                base_sources[path] = "\n".join(lines)
        return base_sources

    def _merge_checked_indexes(
        self,
        indexes: dict[str, FileIndex],
        replacements: dict[str, FileIndex],
    ) -> None:
        for path, index in replacements.items():
            if path not in indexes:
                continue
            index.families = indexes[path].families
            indexes[path] = index

    def _apply_ts_checker(self) -> None:
        if not self._ts_sources:
            return
        base_sources = self._collect_base_ts_sources()
        checked = index_ts_checked(
            getattr(self.repo, "root", os.getcwd()), [self._ts_sources, base_sources]
        )
        if checked is None:
            return
        for indexes, replacements in zip((self.index, self.base_index), checked):
            self._merge_checked_indexes(indexes, replacements)
        self.stats["tsChecker"] = len(checked[0]) == len(self._ts_sources)

    def _update_stats(self, began: float) -> None:
        self.stats.update(
            files=len(self.index) + len(self.workflows) + len(self.terraform),
            symbols=sum(len(index.symbols) for index in self.index.values()),
            calls=sum(len(index.calls) for index in self.index.values()),
            families=sum(len(index.families) for index in self.index.values()),
            workflows=len(self.workflows),
            terraform=len(self.terraform),
            ms=int((self.clock() - began) * 1000),
        )

    def build(self) -> "Reference":
        began = self.clock()
        tracked = set(self.repo.files())
        changed = [item.path for item in self.files if item.path in tracked]
        self._record_diff_metadata()
        self._modules = _module_map(tracked)

        wanted = self._collect_wanted_paths(changed, tracked)
        self._read_wanted_paths(wanted)
        self._resolve_all_imports_and_calls()
        self._index_base_files(changed)
        self._apply_ts_checker()
        self._update_stats(began)

        setattr(self.repo, "reference", self)
        return self

    def _parse_file_by_kind(self, path: str, text: str) -> None:
        if _PY.search(path):
            self.index[path] = index_python(path, text)
        elif _TS.search(path):
            self.index[path] = index_ts(path, text)
            self._ts_sources[path] = text
        elif _WORKFLOW.search(path):
            node = parse_yaml(text)
            if node is not None and isinstance(node.value, dict):
                self.workflows[path] = node
        elif _TF.search(path):
            self.terraform[path] = parse_terraform(path, text)

    def _read(self, path: str) -> None:
        if path in self.index or path in self.workflows or path in self.terraform:
            return
        lines = self.repo.lines(path)
        if lines is None:
            return
        text = "\n".join(lines)
        self.stats["bytes"] += len(text)
        try:
            self._parse_file_by_kind(path, text)
        except Exception:  # one unreadable file never costs the others
            self.stats["skipped"] += 1

    def _grep_importers(self, path: str, stem: str, tracked: set[str], out: list[str]) -> None:
        # `git grep -E` is POSIX: no \s, \w or \b.
        name = re.escape(stem)
        pattern = (
            rf"(from|import)[ \t]+[A-Za-z0-9_.]*{name}([^A-Za-z0-9_]|$)"
            rf"|require\(['\"][^'\"]*{name}['\"]"
            rf"|from[ \t]+['\"][^'\"]*{name}(\.[a-z]+)?['\"]"
        )
        try:
            hits = self.repo.grep(pattern, limit=MAX_IMPORTERS * 2)
        except Exception:
            return
        for hit_path, _line, _text in hits:
            if len(out) >= MAX_IMPORTERS:
                break
            if hit_path != path and hit_path in tracked and _is_code_path(hit_path) and hit_path not in out:
                out.append(hit_path)

    def _importers(self, changed: list[str], tracked: set[str]) -> list[str]:
        """Files that import a changed module, by a grep of its module name or relative path."""
        out: list[str] = []
        for path in changed:
            stem = _module_stem(path)
            if stem is None:
                continue
            self._grep_importers(path, stem, tracked, out)
        return out

    def _changed_exported_names(self, changed: list[str]) -> list[str]:
        names: list[str] = []
        for path in changed:
            file_index = self.index.get(path)
            if file_index is None:
                continue
            exported = [
                symbol.name
                for symbol in file_index.symbols
                if symbol.kind in ("function", "method", "class") and symbol.exported
            ]
            names.extend(exported)
            if len(names) >= 12:
                return names[:12]
        return names

    def _test_files(self, changed: list[str], tracked: set[str]) -> list[str]:
        names = self._changed_exported_names(changed)
        if not names:
            return []
        pattern = "|".join(re.escape(name) for name in names)
        try:
            hits = self.repo.grep(rf"(^|[^A-Za-z0-9_])({pattern})([^A-Za-z0-9_]|$)", limit=200)
        except Exception:
            return []
        out: list[str] = []
        for hit_path, _line, _text in hits:
            if len(out) >= MAX_TEST_FILES:
                break
            is_test = _TEST_PATH.search(hit_path) and _is_code_path(hit_path)
            if is_test and hit_path in tracked and hit_path not in out:
                out.append(hit_path)
        return out

    def _imported_paths(self, index: FileIndex) -> list[str]:
        out: list[str] = []
        for _local, (module_path, name) in index.imports.items():
            resolved = module_path or self._resolve_module(name, index.path)
            if resolved and resolved not in out:
                out.append(resolved)
        reexport_pattern = r"export\s+(?:\*|\{[^}]*\})\s+from\s+['\"]([^'\"]+)['\"]"
        for spec in re.findall(reexport_pattern, self._ts_sources.get(index.path, "")):
            resolved = self._resolve_module(spec, index.path)
            if resolved and resolved not in out:
                out.append(resolved)
        return out

    def _resolve_ts_module(self, spec: str, from_path: str) -> str | None:
        spec = spec.partition(":")[0]
        if not spec.startswith("."):
            return None
        base = posixpath.normpath(posixpath.join(posixpath.dirname(from_path), spec))
        known_paths = set(self._modules.values())
        candidates: list[str] = []
        if base.endswith((".js", ".jsx", ".mjs", ".cjs")):
            stem, ext = posixpath.splitext(base)
            candidates.extend((stem + _TS_JS_EXT_MAP.get(ext, ".ts"), stem + ".tsx"))
        candidates.extend(base + suffix for suffix in _TS_EXTENSIONS)
        return next((candidate for candidate in candidates if candidate in known_paths), None)

    def _resolve_py_relative(self, module: str, name: str, from_path: str) -> str | None:
        dots = len(module) - len(module.lstrip("."))
        directory = posixpath.dirname(from_path)
        for _ in range(dots - 1):
            directory = posixpath.dirname(directory)
        rest = module.lstrip(".")
        base = posixpath.join(directory, rest.replace(".", "/")) if rest else directory
        known_paths = set(self._modules.values())
        candidates = [f"{base}.py", f"{base}/__init__.py"]
        # `from . import name`: the name may be a module.
        if name and name != "*":
            candidates.extend((f"{base}/{name}.py", f"{base}/{name}/__init__.py"))
        return next((candidate for candidate in candidates if candidate in known_paths), None)

    def _resolve_module(self, spec: str, from_path: str) -> str | None:
        """The repository file a Python module (`pkg.mod`, `.rel:name`) or a TS specifier (`./x`) names."""
        from_index = self.index.get(from_path)
        if spec.startswith(("./", "../", "/")) or (from_index is not None and from_index.lang == "ts"):
            return self._resolve_ts_module(spec, from_path)
        module, _, name = spec.partition(":")
        if module.startswith("."):
            return self._resolve_py_relative(module, name, from_path)
        return self._modules.get(module) or (self._modules.get(f"{module}.{name}") if name else None)

    def _build_call_lookups(
        self,
        index: FileIndex,
    ) -> tuple[dict[str, list[Symbol]], dict[str, Symbol], dict[str, list[Symbol]]]:
        by_name: dict[str, list[Symbol]] = {}
        all_callables = [
            symbol
            for other in self.index.values()
            for symbol in other.symbols
            if symbol.kind in ("function", "method", "class")
        ]
        for symbol in all_callables:
            by_name.setdefault(symbol.name, []).append(symbol)
        local = {symbol.name: symbol for symbol in index.symbols if symbol.kind in ("function", "class")}
        methods: dict[str, list[Symbol]] = {}
        for symbol in index.symbols:
            if symbol.kind == "method":
                methods.setdefault(symbol.name, []).append(symbol)
        return by_name, local, methods

    def _resolve_unqualified_call(
        self,
        leaf: str,
        index: FileIndex,
        local: dict[str, Symbol],
    ) -> list[Symbol]:
        if leaf in local:
            return [local[leaf]]
        if leaf not in index.imports:
            return []
        module_path, name = index.imports[leaf]
        if not (module_path and module_path in self.index):
            return []
        return [
            symbol
            for symbol in self.index[module_path].symbols
            if symbol.name == name and symbol.kind in ("function", "class")
        ]

    def _resolve_imported_attr(self, head: str, leaf: str, index: FileIndex) -> list[Symbol]:
        module_path, name = index.imports[head]
        if not (module_path and module_path in self.index):
            return []
        target_symbols = self.index[module_path].symbols
        if name in ("*", "default"):
            return [
                symbol
                for symbol in target_symbols
                if symbol.name == leaf and symbol.kind in ("function", "class", "method")
            ]
        return [symbol for symbol in target_symbols if symbol.parent == name and symbol.name == leaf]

    def _resolve_self_method(
        self,
        call: CallSite,
        leaf: str,
        methods: dict[str, list[Symbol]],
    ) -> list[Symbol]:
        own = call.enclosing.split(".")[0] if call.enclosing else None
        return [symbol for symbol in methods[leaf] if symbol.parent == own] or methods[leaf]

    def _resolve_qualified_call(
        self,
        call: CallSite,
        head: str,
        leaf: str,
        index: FileIndex,
        local: dict[str, Symbol],
        methods: dict[str, list[Symbol]],
    ) -> list[Symbol]:
        if head in ("self", "cls", "this") and leaf in methods:
            return self._resolve_self_method(call, leaf, methods)
        if head in index.imports:
            return self._resolve_imported_attr(head, leaf, index)
        if head in local and local[head].kind == "class":
            return [symbol for symbol in index.symbols if symbol.parent == head and symbol.name == leaf]
        return []

    def _resolve_calls(self, index: FileIndex) -> None:
        by_name, local, methods = self._build_call_lookups(index)
        for call in index.calls:
            parts = call.callee.split(".")
            leaf = parts[-1]
            if len(parts) == 1:
                targets = self._resolve_unqualified_call(leaf, index, local)
            else:
                targets = self._resolve_qualified_call(call, parts[0], leaf, index, local, methods)
            if not targets and len(parts) > 1 and leaf in by_name and len(by_name[leaf]) <= 3:
                targets = by_name[leaf]
            call.targets = [symbol.key for symbol in targets][:5]

    def symbols_named(self, name: str) -> list[Symbol]:
        leaf = name.rsplit(".", 1)[-1]
        aliases = {
            target
            for index in self.index.values()
            if index.checked
            for local, target in index.imports.items()
            if local == leaf
        }
        out = [
            symbol
            for index in self.index.values()
            for symbol in index.symbols
            if symbol.qualname == name or symbol.name == leaf or (symbol.path, symbol.name) in aliases
        ]
        return sorted(
            out,
            key=lambda symbol: (
                symbol.qualname != name,
                symbol.path not in self.changed_lines,
                symbol.path,
                symbol.line,
            ),
        )

    def enclosing(self, path: str, line: int) -> Symbol | None:
        index = self.index.get(path)
        if index is None:
            return None
        matches = [
            symbol
            for symbol in index.symbols
            if symbol.kind in ("function", "method", "class") and symbol.line <= line <= symbol.end
        ]
        if not matches:
            return None
        return max(matches, key=lambda symbol: symbol.line)

    def _classify_caller(
        self,
        index: FileIndex,
        call: CallSite,
        wanted: set[str],
        leaf: str,
        outside_changes: bool,
    ) -> str | None:
        if outside_changes and call.line in self.changed_lines.get(call.path, set()):
            return None
        if wanted and set(call.targets) & wanted:
            return "resolved"
        if not index.checked and not call.targets and call.callee.rsplit(".", 1)[-1] == leaf:
            return "by_name"
        return None

    def callers(self, name: str, *, outside_changes: bool = True) -> list[CallSite]:
        """Call sites reaching a definition of `name`, then unresolved name matches."""
        wanted = {symbol.key for symbol in self.symbols_named(name)}
        leaf = name.rsplit(".", 1)[-1]
        resolved: list[CallSite] = []
        by_name: list[CallSite] = []
        buckets = {"resolved": resolved, "by_name": by_name}
        all_calls = [(index, call) for index in self.index.values() for call in index.calls]
        for index, call in all_calls:
            bucket = self._classify_caller(index, call, wanted, leaf, outside_changes)
            if bucket is not None:
                buckets[bucket].append(call)
        return resolved + by_name

    def callees(self, name: str) -> list[CallSite]:
        for symbol in self.symbols_named(name):
            index = self.index.get(symbol.path)
            if index is None:
                continue
            if index.checked:
                return [call for call in index.calls if call.enclosing == symbol.qualname]
            return [call for call in index.calls if symbol.line < call.line <= symbol.end]
        return []

    def tests_of(self, name: str) -> list[tuple[str, int, str]]:
        """(path, line, test name) of test functions that call `name`."""
        out: list[tuple[str, int, str]] = []
        for call in self.callers(name, outside_changes=False):
            if not _TEST_PATH.search(call.path):
                continue
            entry = (call.path, call.line, call.enclosing or "")
            if entry not in out:
                out.append(entry)
        return out

    def overrides(self, symbol: Symbol) -> list[Symbol]:
        """Methods of the same name in other classes of the index (overrides, or siblings in a family)."""
        if symbol.kind != "method":
            return []
        return [
            other
            for index in self.index.values()
            for other in index.symbols
            if other.kind == "method" and other.name == symbol.name and other.key != symbol.key
        ]

    def family_matches(self, family: Family) -> list[tuple[Family, str]]:
        """Families elsewhere sharing most items: (family, 'same' | 'missing ...')."""
        mine = set(family.items)
        others = [other for index in self.index.values() for other in index.families]
        out: list[tuple[Family, str]] = []
        for other in others:
            comparison = _compare_family(family, mine, other)
            if comparison is not None:
                out.append(comparison)
        return out

    def changed_symbols(self) -> list[Symbol]:
        out: list[Symbol] = []
        for path, lines in self.changed_lines.items():
            index = self.index.get(path)
            if index is None:
                continue
            matched = [
                symbol
                for symbol in index.symbols
                if symbol.kind in ("function", "method", "class")
                and any(symbol.line <= line <= symbol.end for line in lines)
            ]
            out.extend(matched)
        return out

    def signature_before(self, symbol: Symbol) -> str | None:
        base = self.base_index.get(symbol.path)
        if base is None:
            return None
        for other in base.symbols:
            if other.qualname == symbol.qualname:
                return other.signature
        return ""  # new in this change

    def _id(self, prefix: str, key: str) -> str:
        if key not in self._ids:
            number = sum(1 for value in self._ids.values() if value.startswith(prefix)) + 1
            self._ids[key] = f"{prefix}{number}"
        return self._ids[key]

    def _signature_change_label(self, symbol: Symbol) -> str:
        before = self.signature_before(symbol)
        if before == "":
            return " [new in this change]"
        if before is not None and before != symbol.signature:
            return f" [was: {before}]"
        return ""

    def _format_symbol_body(self, symbol: Symbol) -> list[str]:
        source = self._repo_lines(symbol.path)
        end = min(symbol.end, symbol.line + MAX_BODY_LINES - 1, len(source))
        lines = [
            "\n".join(
                f"{number:>5} | {_clip(source[number - 1], 300)}"
                for number in range(symbol.line, end + 1)
            )
        ]
        if end < symbol.end:
            lines.append(f"      ... ({symbol.end - end} more lines)")
        return lines

    def symbol_view(self, symbol: Symbol, *, body: bool = False) -> str:
        """One definition: location, signature, class, overrides, callers, and tests."""
        change = self._signature_change_label(symbol)
        callers = self.callers(symbol.qualname)
        tests = self.tests_of(symbol.qualname)
        overrides = self.overrides(symbol)
        lines = [f"{symbol.path}:{symbol.line}-{symbol.end} {symbol.signature or symbol.name}{change}"]
        if symbol.doc:
            lines.append(f"  doc: {symbol.doc}")
        if symbol.parent:
            lines.append(f"  in class {symbol.parent}")
        if overrides:
            override_text = "; ".join(
                f"{other.path}:{other.line} {other.qualname}" for other in overrides[:4]
            )
            lines.append("  same-named methods: " + override_text)
        lines.append(_format_callers_line(callers))
        if tests:
            test_text = "; ".join(
                f"{path}:{line}" + (f" {name}" if name else "")
                for path, line, name in tests[:MAX_TESTS_SHOWN]
            )
            lines.append("  tests calling it: " + test_text)
        if body:
            lines.extend(self._format_symbol_body(symbol))
        return "\n".join(lines)

    def _collect_hunk_sections(self, seen: set[str]) -> list[str]:
        sections: list[str] = []
        hunk_starts = [
            (item.path, hunk.new_first)
            for item in self.files
            if item.path in self.index
            for hunk in item.hunks
        ]
        for path, line in hunk_starts:
            inner = self.enclosing(path, line)
            if inner is None or inner.key in seen:
                continue
            seen.add(inner.key)
            sections.append(f"[{self._id('D', inner.key)}] " + self.symbol_view(inner))
        return sections

    def block(self, limit: int = BLOCK_CHARS) -> str:
        """Bounded reference view: changed definitions, hunk enclosings, literal
        families, and workflow/Terraform facts."""
        sections: list[str] = []
        seen: set[str] = set()
        for symbol in self.changed_symbols()[:12]:
            if symbol.key not in seen:
                seen.add(symbol.key)
                sections.append(f"[{self._id('D', symbol.key)}] " + self.symbol_view(symbol))
        sections.extend(self._collect_hunk_sections(seen))
        sections.extend(f"[{fact['id']}] {fact['text']}" for fact in self.facts())
        if not sections:
            return ""
        text = (
            "Reference, built by the host from the repository at the reviewed commit "
            "(definitions resolved, not grepped; untrusted repository data). "
            'Ask {"symbol": {"name": ...}} for a definition with its body, '
            '{"callers": {"symbol": ...}}, {"callees": {"symbol": ...}} '
            'and {"tests_of": {"symbol": ...}} for more.'
        )
        for section in sections:
            if len(text) + len(section) + 2 > limit:
                text += "\n... (reference cut at its size limit)"
                break
            text += "\n\n" + section
        return text

    def _format_family_match(self, other: Family, how: str) -> str:
        name_part = f" (`{other.name}`)" if other.name else ""
        diff_part = f": {how}" if how != "same" else ""
        return f"{other.path}:{other.line}{name_part}{diff_part}"

    def _family_fact_entry(self, family: Family, matches: list[tuple[Family, str]]) -> dict[str, Any]:
        label = f"`{family.name}`" if family.name else "a literal list"
        where = "; ".join(self._format_family_match(other, how) for other, how in matches[:6])
        differing = any(how != "same" and other.path not in self.changed_lines for other, how in matches)
        preview = ", ".join(repr(item) for item in family.items[:6])
        ellipsis = ", ..." if len(family.items) > 6 else ""
        tail = ". A file not in this change lists it differently." if differing else "."
        return {
            "id": self._id("F", f"{family.path}:{family.line}"),
            "kind": "family",
            "shape": "family",
            "path": family.path,
            "line": family.line,
            "items": list(family.items),
            "matches": [(other.path, other.line, how) for other, how in matches],
            "text": (
                f"The change edits the literal family {label} at {family.path}:{family.line} "
                f"({preview}{ellipsis}); the same family is listed at {where}{tail}"
            ),
        }

    def _file_family_facts(self, index: FileIndex, lines: set[int]) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        for family in index.families:
            if len(family.items) < MIN_FAMILY_REPORTED:
                continue
            span = range(family.line, family.line + max(1, len(family.items)))
            if family.line not in lines and not any(line in lines for line in span):
                continue
            matches = self.family_matches(family)
            if matches:
                out.append(self._family_fact_entry(family, matches))
        return out

    def _family_facts(self) -> list[dict[str, Any]]:
        facts: list[dict[str, Any]] = []
        for item in self.files:
            index = self.index.get(item.path)
            if index is not None:
                facts.extend(self._file_family_facts(index, self.changed_lines.get(item.path, set())))
        return facts

    def _signature_fact_entry(self, symbol: Symbol, before: str) -> dict[str, Any]:
        callers = self.callers(symbol.qualname)
        caller_summary = (
            f"{len(callers)} caller(s) outside the change: "
            + "; ".join(f"{call.path}:{call.line}" for call in callers[:8])
            if callers
            else "no caller outside the change in the indexed files"
        )
        text = (
            f"The signature of {symbol.qualname} changed: was `{before}`, "
            f"now `{symbol.signature}`; {caller_summary}"
        )
        return {
            "id": self._id("F", f"sig:{symbol.key}"),
            "kind": "signature",
            "shape": "signature",
            "path": symbol.path,
            "line": symbol.line,
            "text": text,
        }

    def _signature_facts(self) -> list[dict[str, Any]]:
        facts: list[dict[str, Any]] = []
        for symbol in self.changed_symbols():
            before = self.signature_before(symbol)
            if before and before != symbol.signature:
                facts.append(self._signature_fact_entry(symbol, before))
        return facts

    def facts(self) -> list[dict[str, Any]]:
        """Host-observed facts about the change: literal families listed elsewhere,
        signature changes with callers, and workflow/Terraform observations."""
        if self._facts is not None:
            return self._facts
        facts: list[dict[str, Any]] = []
        facts.extend(self._family_facts())
        facts.extend(self._signature_facts())
        facts.extend(self.workflow_facts())
        facts.extend(self.terraform_facts())
        self._facts = facts
        return facts

    def workflow_facts(self) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        for path, node in self.workflows.items():
            if path not in self.changed_lines:
                continue
            try:
                out += _workflow_facts(self, path, node)
            except Exception:
                continue
        return out

    def terraform_facts(self) -> list[dict[str, Any]]:
        try:
            return _terraform_facts(self)
        except (OSError, ValueError, TypeError, KeyError, AttributeError, RuntimeError):
            return []

    def _checked_citations(self, fact: dict[str, Any]) -> list[dict[str, Any]]:
        citations: list[dict[str, Any]] = []
        for cite_path, cite_line in fact.get("cite", []):
            source = self._repo_lines(cite_path)
            if 1 <= cite_line <= len(source):
                quote = source[cite_line - 1].strip()
                citations.append({"path": cite_path, "line": cite_line, "quote": quote})
        return citations

    def structural_findings(self) -> list[dict[str, Any]]:
        """Facts that are findings by themselves, with host-checked citations."""
        out: list[dict[str, Any]] = []
        for fact in self.facts():
            finding = fact.get("finding")
            if not finding:
                continue
            citations = self._checked_citations(fact)
            if not citations:
                continue
            beyond_diff = any(
                cite["path"] != fact["path"]
                or cite["line"] not in self.changed_lines.get(cite["path"], set())
                for cite in citations
            )
            entry = {
                **finding,
                "file": fact["path"],
                "line": fact["line"],
                "citations": citations,
                "shape": fact.get("shape", ""),
                "source": "deep:structure",
                "reviewers": ["deep:structure"],
                "beyond_diff": beyond_diff,
                "confidence": finding.get("confidence", 0.7),
            }
            out.append(entry)
        return out


def _module_map(tracked: Iterable[str]) -> dict[str, str]:
    """Dotted module name -> path for Python files under plausible source roots,
    plus every path by itself for TS resolution."""
    out: dict[str, str] = {}
    for path in tracked:
        out[path] = path
        if not _PY.search(path):
            continue
        parts = path[:-3].split("/")
        if parts[-1] == "__init__":
            parts = parts[:-1]
        for start in range(len(parts)):
            dotted = ".".join(parts[start:])
            if dotted and (dotted not in out or start == 0):
                out.setdefault(dotted, path)
    return out


def _serve_symbol(reference: Reference, name: str) -> tuple[str, str] | None:
    symbols = reference.symbols_named(name)
    if not symbols:
        return None
    parts = [reference.symbol_view(symbol, body=True) for symbol in symbols[:3]]
    more = (
        f"\n... {len(symbols) - 3} more definitions of {name.rsplit('.', 1)[-1]}"
        if len(symbols) > 3
        else ""
    )
    return f"symbol {name} -> {len(symbols)} definition(s)", "\n\n".join(parts) + more


def _serve_callers(reference: Reference, name: str) -> tuple[str, str] | None:
    calls = reference.callers(name, outside_changes=False)
    if not calls:
        return None  # a defined name nothing calls: text search finds its uses
    shown = calls[: MAX_CALLERS_SHOWN * 3]
    lines = [
        f"{call.path}:{call.line}"
        + (f" in {call.enclosing}" if call.enclosing else "")
        + (" (resolved)" if call.targets else " (by name)")
        + f": {_clip(call.text, 140)}"
        for call in shown
    ]
    body = "\n".join(lines) or "no call site in the indexed files"
    suffix = " (more not shown)" if len(calls) > len(shown) else ""
    return f"callers {name} -> {len(calls)} call sites{suffix}", body


def _serve_callees(reference: Reference, name: str) -> tuple[str, str] | None:
    symbols = reference.symbols_named(name)
    if not symbols:
        return None
    calls = reference.callees(name)
    lines = [
        f"{call.path}:{call.line}: {call.callee}(...)"
        + (" -> " + ", ".join(call.targets[:3]) if call.targets else " (unresolved)")
        for call in calls[:40]
    ]
    body = "\n".join(lines) or "no calls inside it"
    return f"callees of {symbols[0].qualname} -> {len(calls)} calls", body


def _serve_tests_of(reference: Reference, name: str) -> tuple[str, str] | None:
    if not reference.symbols_named(name) and not reference.callers(name, outside_changes=False):
        return None
    tests = reference.tests_of(name)
    lines = [
        f"{path}:{line}" + (f" {test}" if test else "") for path, line, test in tests[:30]
    ]
    body = "\n".join(lines) or "no test calls it in the indexed files"
    return f"tests_of {name} -> {len(tests)} test(s)", body


_SERVE_HANDLERS: dict[str, Callable[[Reference, str], tuple[str, str] | None]] = {
    "symbol": _serve_symbol,
    "callers": _serve_callers,
    "callees": _serve_callees,
    "tests_of": _serve_tests_of,
}


def serve(reference: Reference, kind: str, args: dict[str, Any]) -> tuple[str, str] | None:
    """Answer a `symbol`, `callers`, `callees`, or `tests_of` request from the
    index: (title, body), or None when unknown."""
    name = args.get("symbol") if kind != "symbol" else (args.get("name") or args.get("symbol"))
    if not isinstance(name, str) or not name.strip():
        return None
    handler = _SERVE_HANDLERS.get(kind)
    if handler is None:
        return None
    return handler(reference, name.strip())
