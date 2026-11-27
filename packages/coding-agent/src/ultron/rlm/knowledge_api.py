"""The knowledge graph, read from the kernel.

The host (`crates/knowledge`) indexes every repository Ultron reviews into one SQLite store: for each git blob, the
constructs it is made of (`sql:trigger:before-update`, `ts:decorator:Controller`, `ts:call:runWithBypass`) and the
symbols it defines; for each revision, its path-to-blob table. This module only reads it, read-only, and only a
schema version it knows; anything else (no store, no such repository or revision, another version) is no
knowledge, never an error, so a review without the store runs as before.

`similar(path)` answers "which other files do what this file does": files of the same language that share the
file's rare constructs, each construct weighted by how rare it is in that revision (inverse document frequency).
Names are not similarity: every Prisma migration is called `migration.sql`, and the one that matters for a new
append-only table is the other migration with a refusing `BEFORE UPDATE` trigger.

`KnowledgeApi` is the REPL's `knowledge`: relations across files read on demand for a task, through the host,
which writes the store (`knowledge.relations`).

`query(sql)` lets an investigator ask the whole repository a structural question in SQL, as CodexGraph's agent
does in Cypher (arXiv 2408.03910): who calls this name, which routes lack this guard, which migrations create a
trigger. It sees only the revision's views (`GRAPH_SCHEMA`), on a connection of its own, through an authorizer
that allows reading those views and nothing else, within a time and row limit; an error comes back as text the
investigator can fix its query from.
"""
from __future__ import annotations

import math
import posixpath
import secrets
import sqlite3
import time
import urllib.parse
from dataclasses import dataclass
from typing import Any

from judge_api import MAX_QUESTIONS, Judge

#: The store schema this reader understands (`knowledge::SCHEMA_VERSION`): 2 added the review notes, 3 the rules
#: (who wrote a note, whether this reviewer missed it, the fates of the findings that cited it), 4 the graph
#: (symbol ranges and parents, calls, imports), 5 the relations across files (`knowledge.relations`, read through
#: the host).
SCHEMA_VERSION = 5

#: Rows a graph query returns, characters of a cell, characters of a query, and how long one may run.
QUERY_ROWS = 30
QUERY_CELL_CHARS = 200
QUERY_CHARS = 2000
QUERY_SECONDS = 5.0

#: The views a graph query reads, each the revision's rows only. `target` is the repository file an import names,
#: when it is one (relative TypeScript specifiers, Python modules).
GRAPH_VIEWS = {
    "files": "SELECT e.path AS path, b.lang AS lang FROM main.entries e JOIN main.blobs b ON b.oid = e.blob"
             " WHERE e.tree = {tree}",
    "symbols": "SELECT e.path AS path, s.name AS name, s.kind AS kind, s.parent AS parent, s.line AS line,"
               " s.end_line AS end_line FROM main.entries e JOIN main.symbols s ON s.blob = e.blob WHERE e.tree = {tree}",
    "calls": "SELECT e.path AS path, c.caller AS caller, c.callee AS callee, c.receiver AS receiver, c.line AS line"
             " FROM main.entries e JOIN main.calls c ON c.blob = e.blob WHERE e.tree = {tree}",
    "imports": "SELECT e.path AS path, i.module AS module, i.name AS name,"
               " resolve_import(e.path, i.module, i.name) AS target, i.line AS line"
               " FROM main.entries e JOIN main.imports i ON i.blob = e.blob WHERE e.tree = {tree}",
    "constructs": "SELECT e.path AS path, c.kind AS kind, c.line AS line, c.count AS count"
                  " FROM main.entries e JOIN main.constructs c ON c.blob = e.blob WHERE e.tree = {tree}",
}

#: The views as the investigators are told them.
GRAPH_SCHEMA = ("files(path, lang); symbols(path, name, kind, parent, line, end_line), kind function, class or "
                "method, parent the symbol containing it; calls(path, caller, callee, receiver, line), caller the "
                "symbol the call is made from, callee the called name; imports(path, module, name, target, line), "
                "target the repository file it names; constructs(path, kind, line, count), kind such as "
                "sql:trigger:before-update, ts:decorator:Get, py:call:execute")

#: Functions a graph query may not call.
_DENIED_FUNCTIONS = {"load_extension", "readfile", "writefile", "edit", "fts3_tokenizer", "zipfile",
                     "sqlar_compress", "sqlar_uncompress"}


class QueryError(Exception):
    """A graph query that could not run: the message says why, for the investigator to fix it."""


def _resolve_js_import(path: str, module: str, paths: set[str]) -> str | None:
    if not module.startswith("."):
        return None
    base = posixpath.normpath(posixpath.join(posixpath.dirname(path), module))
    candidates = [base]
    for ext in (".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"):
        candidates.append(f"{base}{ext}")
    for ext in (".ts", ".tsx", ".js"):
        candidates.append(f"{base}/index{ext}")
    for candidate in candidates:
        if candidate in paths:
            return candidate
    return None


def _resolve_py_import(
    path: str,
    module: str,
    name: str | None,
    paths: set[str],
    modules: dict[str, str],
) -> str | None:
    if not module.startswith("."):
        qualified = modules.get(f"{module}.{name}") if name else None
        return qualified or modules.get(module)
    dots = len(module) - len(module.lstrip("."))
    directory = posixpath.dirname(path)
    for _ in range(dots - 1):
        directory = posixpath.dirname(directory)
    rest = module.lstrip(".")
    base = posixpath.join(directory, rest.replace(".", "/")) if rest else directory
    candidates = [f"{base}.py", f"{base}/__init__.py"]
    # `from . import name`: the name may be a module.
    if name:
        candidates = [f"{base}/{name}.py", f"{base}/{name}/__init__.py"] + candidates
    for candidate in candidates:
        if candidate in paths:
            return candidate
    return None


def _import_resolver(paths: set[str]):
    """`resolve_import(path, module, name)`: the repository file an import of `path` names, or None."""
    modules: dict[str, str] = {}
    for path in sorted(paths):
        if not path.endswith(".py"):
            continue
        parts = path[:-3].split("/")
        if parts[-1] == "__init__":
            parts = parts[:-1]
        for start in range(len(parts)):
            dotted = ".".join(parts[start:])
            if dotted and (dotted not in modules or start == 0):
                modules[dotted] = path

    def resolve(path: Any, module: Any, name: Any) -> str | None:
        if not isinstance(path, str) or not isinstance(module, str) or not module:
            return None
        symbol = (
            name if isinstance(name, str) and name not in ("*", "default") else None
        )
        if not path.endswith(".py"):
            return _resolve_js_import(path, module, paths)
        return _resolve_py_import(path, module, symbol, paths, modules)

    return resolve

#: A rule rejected at least this often, and accepted less than this share of the time, is no longer served.
DISCREDIT_REJECTED = 3
DISCREDIT_SHARE = 0.25
#: What a miss adds to a rule's rank: a person found there what this reviewer did not.
MISS_BONUS = 0.5

#: A construct shared by more than this share of a language's files says nothing about similarity.
COMMON_SHARE = 0.5


@dataclass
class Note:
    """A review comment left on a pull request of the repository, by anyone (a teammate, a bot, this reviewer)."""
    pull: int
    path: str
    line: int | None
    author: str
    body: str
    url: str
    at: str
    #: `addressed` (its thread was resolved), `outdated` (a later commit changed the lines, unresolved) or `open`.
    state: str


@dataclass
class Rule:
    """A person's addressed review comment, as a rule the investigators can cite (`R<id>`), with its credit."""
    id: int
    note: Note
    #: The fates of the findings that cited it.
    accepted: int
    rejected: int
    #: This reviewer reviewed that pull request and raised nothing there.
    missed: bool
    #: The file it was left on, when it is not the file asked about: how much that file is like it.
    alike: "Similar | None" = None

    @property
    def credit(self) -> float:
        """Its rank: the share of citing findings accepted (Laplace), more for a miss."""
        return (self.accepted + 1) / (self.accepted + self.rejected + 2) + (MISS_BONUS if self.missed else 0.0)

    @property
    def discredited(self) -> bool:
        return self.rejected >= DISCREDIT_REJECTED and self.accepted < DISCREDIT_SHARE * (self.accepted + self.rejected)


@dataclass
class Similar:
    path: str
    score: float
    #: The shared constructs, rarest first; among equally rare ones, the most specific first.
    shared: list[str]


def _rank_similar(
    shared: dict[str, list[str]],
    weight: dict[str, float],
    limit: int,
) -> list[Similar]:
    ranked: list[Similar] = []
    for other, kinds in shared.items():
        # `sql:trigger:before-update` says more than `sql:trigger`, which every such file also has.
        kinds.sort(key=lambda kind: (-weight[kind], -kind.count(":"), kind))
        score = round(sum(weight[kind] for kind in kinds), 3)
        ranked.append(Similar(other, score, kinds))
    ranked.sort(key=lambda item: (-item.score, len(item.shared), item.path))
    return ranked[:limit]


def _prepare_graph_views(
    conn: sqlite3.Connection, tree: int, sql: str
) -> set[str]:
    if "imports" in sql.lower():
        paths = {
            str(path)
            for (path,) in conn.execute(
                "SELECT path FROM entries WHERE tree = ?", (tree,)
            )
        }
        conn.create_function(
            "resolve_import", 3, _import_resolver(paths), deterministic=True
        )
    else:
        conn.create_function(
            "resolve_import", 3, lambda *_: None, deterministic=True
        )
    # Two layers prevent query CTEs from shadowing view names and bypassing table isolation.
    hidden = {name: f"g{secrets.token_hex(8)}_{name}" for name in GRAPH_VIEWS}
    for name, body in GRAPH_VIEWS.items():
        conn.execute(
            f"CREATE TEMP VIEW {hidden[name]} AS {body.format(tree=int(tree))}"
        )
        conn.execute(
            f"CREATE TEMP VIEW {name} AS SELECT * FROM temp.{hidden[name]}"
        )
    return set(hidden.values())


def _make_query_authorizer(readers: set[str]):
    def authorize(
        action: int, _first: Any, second: Any, database: Any, source: Any
    ) -> int:
        if action in (sqlite3.SQLITE_SELECT, sqlite3.SQLITE_RECURSIVE):
            return sqlite3.SQLITE_OK
        if action == sqlite3.SQLITE_READ:
            # The views, and the tables only through them: no other revision, no review notes. A
            # common table expression of the query has no database.
            allowed = database is None or database == "temp" or source in readers
            return sqlite3.SQLITE_OK if allowed else sqlite3.SQLITE_DENY
        if action == sqlite3.SQLITE_FUNCTION:
            denied = str(second).lower() in _DENIED_FUNCTIONS
            return sqlite3.SQLITE_DENY if denied else sqlite3.SQLITE_OK
        return sqlite3.SQLITE_DENY

    return authorize


def _run_graph_sql(
    conn: sqlite3.Connection, sql: str, limit: int, seconds: float
) -> tuple[list[str], list[tuple[Any, ...]]]:
    deadline = time.monotonic() + seconds
    conn.set_progress_handler(lambda: 1 if time.monotonic() > deadline else 0, 2000)
    try:
        cursor = conn.execute(sql)
        if not cursor.description:
            raise QueryError("a query must be a SELECT that returns rows")
        columns = [str(item[0]) for item in cursor.description]
        rows = cursor.fetchmany(limit + 1)
        return columns, rows
    except sqlite3.Warning:
        raise QueryError("one statement only, without a trailing one") from None
    except sqlite3.DatabaseError as error:
        message = str(error)
        if "interrupted" in message:
            raise QueryError(
                f"the query ran longer than {seconds:g}s: narrow it "
                "(a WHERE on path or name, fewer joins)"
            ) from None
        if "not authorized" in message or "prohibited" in message:
            raise QueryError(
                "not authorized: a query reads only the views files, symbols, "
                "calls, imports and constructs"
            ) from None
        raise QueryError(f"{message}. The views: {GRAPH_SCHEMA}") from None


def _truncate_cell(value: Any) -> Any:
    if isinstance(value, str) and len(value) > QUERY_CELL_CHARS:
        return value[:QUERY_CELL_CHARS] + "..."
    if isinstance(value, bytes):
        return f"<{len(value)} bytes>"
    return value


def _format_query_rows(
    rows: list[tuple[Any, ...]], limit: int
) -> list[tuple[Any, ...]]:
    formatted: list[tuple[Any, ...]] = []
    for row in rows[:limit]:
        formatted.append(tuple(_truncate_cell(value) for value in row))
    return formatted


class Knowledge:
    """One repository at one revision, in the store."""

    def __init__(self, conn: sqlite3.Connection, tree: int, repo: int = 0, db: str = "") -> None:
        self._conn = conn
        self._tree = tree
        self._repo = repo
        self._db = db

    @classmethod
    def open(cls, db: Any, repo: Any, rev: Any) -> "Knowledge | None":
        """The revision `rev` of `repo` in the store at `db`; None when any of them is missing or unreadable."""
        if not all(isinstance(value, str) and value for value in (db, repo, rev)):
            return None
        try:
            conn = sqlite3.connect(f"file:{urllib.parse.quote(db)}?mode=ro", uri=True, timeout=5)
        except sqlite3.Error:
            return None
        try:
            row = conn.execute("SELECT value FROM meta WHERE key = 'schema_version'").fetchone()
            if row is None or str(row[0]) != str(SCHEMA_VERSION):
                conn.close()
                return None
            tree = conn.execute(
                "SELECT t.id, t.repo FROM trees t JOIN repos r ON r.id = t.repo WHERE r.name = ? AND t.rev = ?",
                (repo, rev),
            ).fetchone()
        except sqlite3.Error:
            conn.close()
            return None
        if tree is None:
            conn.close()
            return None
        return cls(conn, int(tree[0]), int(tree[1]), db)

    def close(self) -> None:
        self._conn.close()

    def constructs(self, path: str) -> list[tuple[str, int]]:
        """(kind, first line) of the file at `path`."""
        try:
            return [(str(kind), int(line)) for kind, line in self._conn.execute(
                "SELECT c.kind, c.line FROM entries e JOIN constructs c ON c.blob = e.blob"
                " WHERE e.tree = ? AND e.path = ? ORDER BY c.line, c.kind",
                (self._tree, path),
            )]
        except sqlite3.Error:
            return []

    def _construct_weights(self, path: str) -> tuple[str, dict[str, float]] | None:
        own = self._conn.execute(
            "SELECT e.blob, b.lang FROM entries e JOIN blobs b ON b.oid = e.blob "
            "WHERE e.tree = ? AND e.path = ?",
            (self._tree, path),
        ).fetchone()
        if own is None:
            return None
        blob, lang = own
        mine = [
            str(kind)
            for (kind,) in self._conn.execute(
                "SELECT kind FROM constructs WHERE blob = ?", (blob,)
            )
        ]
        if not mine:
            return None
        (files,) = self._conn.execute(
            "SELECT COUNT(*) FROM entries e JOIN blobs b ON b.oid = e.blob "
            "WHERE e.tree = ? AND b.lang = ?",
            (self._tree, lang),
        ).fetchone()
        marks = ",".join("?" * len(mine))
        frequency = dict(
            self._conn.execute(
                f"SELECT c.kind, COUNT(*) FROM entries e JOIN blobs b ON b.oid = e.blob"
                f" JOIN constructs c ON c.blob = e.blob"
                f" WHERE e.tree = ? AND b.lang = ? AND c.kind IN ({marks}) GROUP BY c.kind",
                (self._tree, lang, *mine),
            ).fetchall()
        )
        weight: dict[str, float] = {}
        for kind, count in frequency.items():
            if count <= 1 or count > COMMON_SHARE * files:
                continue
            score = math.log((files + 1) / (count + 1))
            if score > 0:
                weight[kind] = score
        return (lang, weight) if weight else None

    def similar(
        self, path: str, *, limit: int = 3, exclude: set[str] | None = None
    ) -> list[Similar]:
        """The files most like `path` by shared rare constructs, best first; [] when nothing is shared."""
        excluded = set(exclude or ()) | {path}
        try:
            weighted = self._construct_weights(path)
            if weighted is None:
                return []
            lang, weight = weighted
            rare = sorted(weight)
            marks = ",".join("?" * len(rare))
            shared: dict[str, list[str]] = {}
            rows = self._conn.execute(
                f"SELECT e.path, c.kind FROM entries e JOIN blobs b ON b.oid = e.blob"
                f" JOIN constructs c ON c.blob = e.blob"
                f" WHERE e.tree = ? AND b.lang = ? AND c.kind IN ({marks})",
                (self._tree, lang, *rare),
            )
            for other, kind in rows:
                if other not in excluded:
                    shared.setdefault(str(other), []).append(str(kind))
        except sqlite3.Error:
            return []
        return _rank_similar(shared, weight, limit)

    def query(
        self,
        sql: Any,
        *,
        limit: int = QUERY_ROWS,
        seconds: float = QUERY_SECONDS,
    ) -> tuple[list[str], list[tuple[Any, ...]], bool]:
        """One read-only SELECT over the revision's graph (`GRAPH_VIEWS`): (columns, rows, whether more rows were
        left out). Raises QueryError with a message the investigator can act on."""
        if not isinstance(sql, str) or not sql.strip() or len(sql) > QUERY_CHARS or "\0" in sql:
            raise QueryError(
                f"a query is one SQL SELECT of at most {QUERY_CHARS} characters"
            )
        try:
            conn = sqlite3.connect(
                f"file:{urllib.parse.quote(self._db)}?mode=ro", uri=True, timeout=5
            )
        except sqlite3.Error as error:
            raise QueryError(f"the graph is not available: {error}") from None
        try:
            readers = _prepare_graph_views(conn, self._tree, sql)
            conn.set_authorizer(_make_query_authorizer(readers))
            columns, rows = _run_graph_sql(conn, sql, limit, seconds)
        finally:
            conn.close()
        return columns, _format_query_rows(rows, limit), len(rows) > limit

    def has_notes(self) -> bool:
        """Whether the store holds any review note of this repository."""
        try:
            return self._conn.execute("SELECT 1 FROM notes WHERE repo = ? LIMIT 1", (self._repo,)).fetchone() is not None
        except sqlite3.Error:
            return False

    def notes_on(self, path: str, *, limit: int = 8) -> list[Note]:
        """Review comments left on `path` in earlier pull requests, addressed ones first, newest first."""
        return self._notes("n.path = ?", (path,), limit)

    def notes_like(self, path: str, *, limit: int = 8, exclude: set[str] | None = None) -> list[tuple[Note, Similar]]:
        """Addressed review comments on the files most like `path` (`similar`): what reviewers asked of code that
        does what this file does, with the constructs the two files share. Best match first."""
        out: list[tuple[Note, Similar]] = []
        for alike in self.similar(path, limit=6, exclude=exclude):
            for note in self._notes("n.path = ? AND n.state = 'addressed'", (alike.path,), limit):
                out.append((note, alike))
        # The closest file first; among its notes, the newest.
        out.sort(key=lambda pair: pair[0].at, reverse=True)
        out.sort(key=lambda pair: -pair[1].score)
        return out[:limit]

    def has_rules(self) -> bool:
        """Whether the store holds a rule of this repository: a person's addressed comment."""
        try:
            return self._conn.execute("SELECT 1 FROM notes WHERE repo = ? AND human = 1 AND state = 'addressed' LIMIT 1",
                                      (self._repo,)).fetchone() is not None
        except sqlite3.Error:
            return False

    def rules_for(self, path: str, *, limit: int = 8, exclude: set[str] | None = None) -> list[Rule]:
        """The rules for `path`: people's addressed comments on it and on the files most like it (`similar`), the
        discredited left out; the best credited first, then this file's, the closest file's, the newest."""
        rules = self._rules(path, None)
        for alike in self.similar(path, limit=6, exclude=exclude):
            rules += self._rules(alike.path, alike)
        rules = [rule for rule in rules if not rule.discredited]
        rules.sort(key=lambda rule: rule.note.at, reverse=True)
        rules.sort(key=lambda rule: -(rule.alike.score if rule.alike else math.inf))
        rules.sort(key=lambda rule: -rule.credit)
        return rules[:limit]

    def _rules(self, path: str, alike: "Similar | None") -> list[Rule]:
        try:
            rows = self._conn.execute(
                "SELECT n.id, n.pull, n.path, n.line, n.author, n.body, n.url, n.at, n.state, n.accepted, n.rejected,"
                " n.missed FROM notes n WHERE n.repo = ? AND n.path = ? AND n.human = 1 AND n.state = 'addressed'"
                " ORDER BY n.at DESC LIMIT 20",
                (self._repo, path),
            ).fetchall()
        except sqlite3.Error:
            return []
        return [Rule(int(rid), Note(int(pull), str(at_path), None if line is None else int(line), str(author), str(body),
                                    str(url), str(at), str(state)), int(accepted), int(rejected), bool(missed), alike)
                for rid, pull, at_path, line, author, body, url, at, state, accepted, rejected, missed in rows]

    def _notes(self, where: str, args: tuple[Any, ...], limit: int) -> list[Note]:
        try:
            rows = self._conn.execute(
                "SELECT n.pull, n.path, n.line, n.author, n.body, n.url, n.at, n.state FROM notes n"
                f" WHERE n.repo = ? AND {where}"
                " ORDER BY n.state = 'addressed' DESC, n.at DESC LIMIT ?",
                (self._repo, *args, limit),
            ).fetchall()
        except sqlite3.Error:
            # A store from before notes (schema 1) has none.
            return []
        return [Note(int(pull), str(path), None if line is None else int(line), str(author), str(body), str(url),
                     str(at), str(state)) for pull, path, line, author, body, url, at, state in rows]


def construct_label(kind: str) -> str:
    """`sql:trigger:before-update` as `trigger before-update`: the language prefix is the file's own."""
    _, _, rest = kind.partition(":")
    return rest.replace(":", " ") if rest else kind


#: The relation kinds `knowledge.relations` reads (crates/knowledge `relations::Kind`).
RELATION_KINDS = ("reads_config", "serves_route", "touches_table", "tests")

#: Proposed relations asked of the judge in one `judge.decide`: as many questions as it takes.
JUDGE_BATCH = MAX_QUESTIONS


def _normalize_relation_args(
    paths: Any, kinds: Any
) -> tuple[list[str] | None, list[str] | None]:
    norm_paths = [paths] if isinstance(paths, str) else paths
    norm_kinds = [kinds] if isinstance(kinds, str) else kinds
    for kind in norm_kinds or ():
        if kind not in RELATION_KINDS:
            allowed = ", ".join(RELATION_KINDS)
            raise ValueError(
                f"knowledge.relations kind {kind!r} is not one of {allowed}"
            )
    path_list = None if norm_paths is None else [str(path) for path in norm_paths]
    kind_list = None if norm_kinds is None else list(norm_kinds)
    return path_list, kind_list


def _apply_relation_verdicts(
    relations: list[dict[str, Any]],
    verdicts: list[dict[str, Any]],
    pruned: dict[str, Any],
) -> list[dict[str, Any]]:
    by_id = {verdict["id"]: verdict for verdict in verdicts}
    kept: list[dict[str, Any]] = []
    for relation in relations:
        verdict = by_id.get(relation["id"])
        if verdict is not None:
            relation = {
                **relation,
                "status": verdict["status"],
                "checked_by": verdict["backend"],
                "rung": verdict["rung"],
                "model": verdict["model"],
                "confidence": verdict["confidence"],
            }
        if relation["status"] == "rejected":
            pruned["rejected"] = pruned.get("rejected", 0) + 1
            continue
        kept.append(relation)
    return kept


def _group_pending_by_path(
    pending: list[dict[str, Any]],
) -> dict[str, list[dict[str, Any]]]:
    by_path: dict[str, list[dict[str, Any]]] = {}
    for item in pending:
        by_path.setdefault(item["path"], []).append(item)
    return by_path


def _judge_group_payload(
    group: list[dict[str, Any]],
) -> tuple[str, dict[str, Any]]:
    state_parts: list[str] = []
    questions: dict[str, Any] = {}
    for index, item in enumerate(group):
        state_parts.append(
            f"[{index}] {item['path']} line {item['line']}:\n{item['context']}"
        )
        questions[f"r{index}"] = {
            "type": "yesno",
            "instructions": f"Snippet [{index}]: {item['question']}",
        }
    return "\n\n".join(state_parts), questions


def _apply_batch_verdict(
    group: list[dict[str, Any]],
    verdict: dict[str, Any],
    spent: dict[str, Any],
    out: list[dict[str, Any]],
) -> None:
    spent["calls"] += verdict["calls"]
    cost = verdict["cost_usd"]
    if cost is None or spent["cost_usd"] is None:
        spent["cost_usd"] = None
    else:
        spent["cost_usd"] += cost
    if verdict["abstained"]:
        spent["abstained"] += len(group)
        return
    for index, item in enumerate(group):
        answer = verdict["answers"].get(f"r{index}")
        if answer is None:
            continue
        if verdict["review"]:
            status = "review"
        else:
            status = "accepted" if answer["value"] is True else "rejected"
        out.append({
            "id": item["id"],
            "status": status,
            "backend": verdict["backend"],
            "rung": verdict["rung"],
            "model": verdict["model"],
            "confidence": answer.get("confidence"),
            "receipt": verdict["receipt"],
        })


class KnowledgeApi:
    """Relations across the files of this repository at `HEAD`, read when a task needs them:
    `found = await knowledge.relations(paths=["crates/judge"], kinds=["reads_config"])`.

    Kinds: `reads_config` (code -> an environment variable or setting it reads), `serves_route` (a handler ->
    `METHOD /path`), `touches_table` (code -> a table its SQL reads, writes or defines), `tests` (a test file -> a
    repository file it imports). Plain code proposes them and settles what nothing else could mean; what only looks
    like one (`config.get("x")`, lowercase SQL in a string) is asked of the judge (`judge.decide`, kind
    "relation"), unless `judge=False`. Proposals and verdicts are cached by file content: asking again costs
    nothing until the files change.

    Returns `{"rev", "relations", "pruned", "files", "read", "truncated", "judge", "mirror"}`:
    - `relations`: `{"id", "kind", "subject", "object", "path", "line", "evidence", "quote", "status",
      "checked_by", "rung", "model", "confidence"}`, in path and line order. `subject` is `path::definition` (or
      the path); `evidence` is `repo://<path>#L<line>`; `status` is "accepted" (by `checked_by`: "code" or the
      judge's backend), "review" (the judge was unsure: check it yourself) or "proposed" (not judged: `judge=False`
      or the judge abstained).
    - `pruned`: counts dropped by plain code and the judge (`kind`, `unresolved` test imports, `rejected`,
      `duplicate`); `files` in scope, `read` this time; `truncated` when the scope held more files than one call
      reads (narrow `paths`).
    - `judge`: `{"calls", "cost_usd", "abstained"}` of this call's judge decisions.
    - `mirror`: the Neo4j mirror's outcome (`{"mirrored"}` or `{"error"}`), None without `graph.neo4j.url`.

    `await knowledge.mirror()` rewrites the Neo4j mirror from the store (and memorantado's memories and their
    links, when memory lives there): `{"relations", "memories", "errors"}`."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge
        self._judge = Judge(bridge)

    async def relations(
        self, paths: Any = None, kinds: Any = None, *, judge: bool = True
    ) -> dict[str, Any]:
        """The relations among the files under `paths` (files or directories; all when None) of `kinds` (all when
        None); see `help(knowledge)`."""
        path_list, kind_list = _normalize_relation_args(paths, kinds)
        found = await self._bridge.request(
            "knowledge.relations",
            {"paths": path_list, "kinds": kind_list},
        )
        relations = list(found["relations"])
        pruned = dict(found["pruned"])
        mirror = found["mirror"]
        spent: dict[str, Any] = {"calls": 0, "cost_usd": 0.0, "abstained": 0}
        verdicts = await self._judged(found["pending"], spent) if judge else []
        if verdicts:
            settled = await self._bridge.request(
                "knowledge.settle",
                {"rev": found["rev"], "verdicts": verdicts},
            )
            if settled["mirror"] is not None:
                mirror = settled["mirror"]
            relations = _apply_relation_verdicts(relations, verdicts, pruned)
        return {
            "rev": found["rev"],
            "relations": relations,
            "pruned": pruned,
            "files": found["files"],
            "read": found["read"],
            "truncated": found["truncated"],
            "judge": spent,
            "mirror": mirror,
        }

    async def _judged(
        self, pending: list[dict[str, Any]], spent: dict[str, Any]
    ) -> list[dict[str, Any]]:
        """Asks the judge about proposed relations, a file's at a time: the verdicts to settle."""
        by_path = _group_pending_by_path(pending)
        verdicts: list[dict[str, Any]] = []
        for items in by_path.values():
            for start in range(0, len(items), JUDGE_BATCH):
                group = items[start : start + JUDGE_BATCH]
                state, questions = _judge_group_payload(group)
                verdict = await self._judge.decide(state, questions, kind="relation")
                _apply_batch_verdict(group, verdict, spent, verdicts)
        return verdicts

    async def mirror(self) -> dict[str, Any]:
        """Rewrites the Neo4j mirror (`graph.neo4j.url`) from the store and memorantado; see `help(knowledge)`."""
        return await self._bridge.request("knowledge.mirror", {})

    def __repr__(self) -> str:
        return "<knowledge: relations(paths=None, kinds=None, *, judge=True), mirror()>"
