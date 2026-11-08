"""Find and read Ultron sessions from the REPL (read-only), through the `sessions` API.

Load with `s = runpy.run_path(f"{skill_dir}/scripts/sessions.py", init_globals={"sessions": sessions})`, then
`await s["session_files"](...)`, `await s["grep_sessions"](rows, pattern)` and `await s["read_session"](row)`.

The host reads every session store this session can see (this project's, the agent dir's `experimental/sessions/`
and Pi-format `sessions/`, one folder per working directory) and follows each session's main conversation (the
branch of lane `main`; inference-frame lanes in the same file are left out). For a turn-level search, call the API
itself: `await sessions.search(pattern, days=7)` returns hits with a `span` that `await sessions.read(span,
around=n)` reads back; cite findings by their span.
"""


def _api():
    api = globals().get("sessions")
    if api is None:
        raise RuntimeError('load sessions.py with init_globals={"sessions": sessions}')
    return api


def _id(session):
    return session["id"] if isinstance(session, dict) else session


async def session_files(cwd=None, days=7, all_projects=False, limit=200):
    """Sessions modified in the last `days` days, newest first: `[{id, cwd, path, created, modified, parent, name,
    first_message, messages, current}]` (`current` marks this session, `parent` a subagent's session).

    Scoped to one working directory (default: the current one) unless `all_projects` is True.
    """
    return await _api().list(cwd="all" if all_projects else cwd, days=days, limit=limit)


async def read_session(session, tool_chars=2000, limit=1000):
    """{path, id, cwd, parent, name, current, span, truncated, messages: [{role, text, tool, ts, entry}]} for the
    session's main conversation (`session`: a row of `session_files`, or an id).

    Tool results are cut to `tool_chars`; thinking is left out; at most `limit` messages (`truncated` says more
    were left). `parent` is set for a subagent's own session.
    """
    read = await _api().read(session=_id(session), tool_chars=tool_chars, limit=limit)
    meta = read["session"]
    return {
        "path": meta["path"],
        "id": meta["id"],
        "cwd": meta["cwd"],
        "parent": meta["parent"],
        "name": meta["name"],
        "current": meta["current"],
        "span": read["span"],
        "truncated": read["truncated"],
        "messages": [
            {"role": e["role"], "text": e["text"], "tool": e["tool"], "ts": e["timestamp"], "entry": e["id"]}
            for e in read["entries"]
        ],
    }


async def grep_sessions(files, pattern, limit=1000):
    """[(id, count)] of the sessions among `files` (rows or ids) whose conversation matches the regex `pattern`
    (case-insensitive), most matches first; at most `limit` matching messages are counted."""
    ids = [_id(f) for f in files]
    if not ids:
        return []
    found = await _api().search(pattern, sessions=ids, cwd="all", limit=limit, per_session=limit)
    counts = {}
    for hit in found["hits"]:
        counts[hit["session"]] = counts.get(hit["session"], 0) + hit["matches"]
    return sorted(counts.items(), key=lambda item: -item[1])
