"""`sessions`: stored Ultron sessions listed, searched and read, read-only, by the Rust host. Findings cite their
turns as spans, `session://<id>#<from>..<to>`, which `sessions.read` reads back.
"""

from __future__ import annotations

import os
from typing import Any


def _cwd(cwd: Any) -> str | None:
    if cwd is None or cwd == "all":
        return cwd
    path = os.fspath(cwd)
    if not path:
        raise ValueError('sessions cwd must be a path or "all"')
    return os.path.abspath(path)


def _strings(value: Any, name: str) -> list[str] | None:
    if value is None:
        return None
    if isinstance(value, str):
        value = [value]
    items = list(value)
    if not all(isinstance(item, str) and item for item in items):
        raise ValueError(f"sessions {name} must be nonempty strings")
    return items


def _payload(**fields: Any) -> dict[str, Any]:
    return {key: value for key, value in fields.items() if value is not None}


class Sessions:
    """Your stored sessions, read-only. Scope defaults to this project's sessions (`cwd="all"`: every project's;
    read other projects' only when the user asks). Every call is awaited.

    - `await sessions.list(days=7)`: sessions newest first, `[{id, cwd, path, created, modified, parent, name,
      first_message, messages, current}]` (`parent` is set for a subagent's session, `current` for this one).
    - `await sessions.search("retry|backoff", days=7)` (a Python-style regex, case-insensitive unless
      `ignore_case=False`) or `sessions.search(terms=["retry", "jitter"])` (every term in one entry): `{hits:
      [{session, cwd, entry, seq, timestamp, role, tool, snippet, matches, span}], searched, truncated, errors,
      current}`, newest session first, at most `limit` hits and `per_session` per session.
    - `await sessions.read(hit["span"], around=5)` or `sessions.read(session=id, from_entry=a, to_entry=b)`: `{session:
      {id, cwd, path, created, modified, parent, name, current}, span, entries: [{id, seq, timestamp, type, role,
      tool, text}], truncated}`. No arguments: this session's whole conversation.

    A span, `session://<id>#<from>..<to>`, names entries `from` through `to` of one session: cite it with a finding.
    Entries are the conversation's branch (`branch="main"`); `branch="all"` searches every entry of a file (other
    lanes, abandoned branches). Text is user and assistant text, tool calls as `[name]` and their code, tool
    results (cut to `tool_chars` when read), compaction and branch summaries; thinking is left out."""

    def __init__(self, bridge: Any) -> None:
        self._bridge = bridge

    async def list(self, *, cwd: Any = None, days: float | None = None, limit: int | None = None) -> list[dict]:
        """Sessions of this project (`cwd=` another path, or "all"), modified in the last `days` days if given,
        newest first, at most `limit` (default 50)."""
        reply = await self._bridge.request("sessions.list", _payload(cwd=_cwd(cwd), days=days, limit=limit))
        return reply["sessions"]

    async def search(self, pattern: str | None = None, *, terms: Any = None, cwd: Any = None,
                     days: float | None = None, sessions: Any = None, branch: str | None = None,
                     roles: Any = None, limit: int | None = None, per_session: int | None = None,
                     ignore_case: bool | None = None, snippet: int | None = None) -> dict[str, Any]:
        """Entries matching `pattern` (a regex) or containing every one of `terms`, in this project's sessions
        (`cwd=` another path, or "all"), optionally only `sessions=[ids]`, those modified in the last `days`, and
        `roles=["user", "assistant", "toolResult", ...]`. Each hit carries a `snippet` around its first match and
        its `span` for `sessions.read`. Limits: `limit` hits (default 50), `per_session` (default 10), `snippet`
        characters (default 300); `truncated` says a limit ended the search early."""
        if (pattern is None) == (terms is None):
            raise ValueError("sessions.search takes a pattern or terms=[...], not both")
        if pattern is not None and (not isinstance(pattern, str) or not pattern):
            raise ValueError("sessions.search pattern must be a nonempty string")
        if ignore_case is not None and not isinstance(ignore_case, bool):
            raise ValueError("sessions.search ignore_case must be True or False")
        return await self._bridge.request("sessions.search", _payload(
            pattern=pattern, terms=_strings(terms, "terms"), cwd=_cwd(cwd), days=days,
            sessions=_strings(sessions, "sessions"), branch=branch, roles=_strings(roles, "roles"), limit=limit,
            per_session=per_session, ignore_case=ignore_case, snippet=snippet))

    async def read(self, span: str | None = None, *, session: str | None = None, from_entry: str | None = None,
                   to_entry: str | None = None, around: int | None = None, branch: str | None = None,
                   limit: int | None = None, tool_chars: int | None = None) -> dict[str, Any]:
        """A span of one session, oldest entry first: `span` ("session://<id>#<from>..<to>", a hit's `span`), or
        `session` (default: this one) from `from_entry` (default: the start) to `to_entry` (default: the end).
        `around=n` adds n entries on each side; at most `limit` entries (default 200); tool results cut to
        `tool_chars` (default 2000). The returned `span` names exactly the entries returned."""
        return await self._bridge.request("sessions.read", _payload(
            span=span, session=session, from_entry=from_entry, to_entry=to_entry, around=around, branch=branch,
            limit=limit, tool_chars=tool_chars))

    def __repr__(self) -> str:
        return ("<sessions: list(*, cwd, days, limit), search(pattern | terms=[...], *, cwd, days, sessions, "
                "branch, roles, limit, per_session), read(span | session=, from_entry=, to_entry=, *, around, "
                "limit, tool_chars)>")
