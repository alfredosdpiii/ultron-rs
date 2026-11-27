"""The deep pass of `ultron autoreview`: investigation beyond the diff, by retrieval.

The fast pass (autoreview_api.py) reviews the changed lines. Many real defects are only visible
from surrounding code: a helper returning a sentinel instead of raising, a parametrize list
missing new members, or a comment claiming what the code does not do.

1. Map (no model): extract symbols, constants, env vars, flags, tables, and claims from the
   diff, then build a bounded investigation brief with file:line anchors from the head commit.
2. Investigators: tool-less frames per lens (claims, siblings, deployment, tests, inputs)
   issue read-only requests (read, grep, list, definition, references, history) served by the
   host over a bounded number of rounds.
3. Evidence: every finding cites file:line with a quoted source line verified by the host;
   findings with missing or mismatched quotes are dropped.

Read-only by construction: models have no tools, and the host runs only `git grep`,
`git show <rev>:<path>`, `git ls-tree`, and `git log` (see `Repo`).
"""
from __future__ import annotations

import asyncio
import json
import re
from dataclasses import dataclass
from typing import Any, Callable

import autoreview_reference as reference_module
import autoreview_tests as testing
from autoreview_deep_brief import (
    _BEHAVIOUR as _BEHAVIOUR,
    _COMMENT as _COMMENT,
    _CONFIG_FILE as _CONFIG_FILE,
    _CONFIG_KEY as _CONFIG_KEY,
    _CONFIG_PATH as _CONFIG_PATH,
    _CONSTANT as _CONSTANT,
    _DESCRIPTION as _DESCRIPTION,
    _DOCLIKE as _DOCLIKE,
    _ENV as _ENV,
    _FIELD as _FIELD,
    _FLAG as _FLAG,
    _INPUT_HINT as _INPUT_HINT,
    _REQUEST_INPUT as _REQUEST_INPUT,
    _RISK as _RISK,
    _STRUCTURE as _STRUCTURE,
    _TABLE as _TABLE,
    _WORD as _WORD,
    Brief as Brief,
    build_brief as build_brief,
    extract as extract,
)
from autoreview_deep_findings import (
    _nearly_identical as _nearly_identical,
    _record as _record,
    _test_line as _test_line,
    assurance as assurance,
    check_citations as check_citations,
    cited_windows as cited_windows,
    merge as merge,
    merge_rules as merge_rules,
    normalize_deep as normalize_deep,
    prove_unpinned as prove_unpinned,
    refuted_leads as refuted_leads,
    regression_findings as regression_findings,
)
from autoreview_deep_investigate import (
    LAST_ROUND as LAST_ROUND,
    SHAPE_LENS as SHAPE_LENS,
    _mentions as _mentions,
    investigate as investigate,
    shapes_by_lens as shapes_by_lens,
)
from autoreview_deep_repo import (
    ALLOWED_GIT as ALLOWED_GIT,
    BRIEF_CHARS as BRIEF_CHARS,
    CITATION_SLACK as CITATION_SLACK,
    CITED_WINDOW as CITED_WINDOW,
    DEDUPE_WINDOW as DEDUPE_WINDOW,
    DEEP_CONTRACT as DEEP_CONTRACT,
    DEFAULT_GREP_HITS as DEFAULT_GREP_HITS,
    DEFAULT_ROUNDS as DEFAULT_ROUNDS,
    DIFF_CHARS as DIFF_CHARS,
    GIT_TIMEOUT_S as GIT_TIMEOUT_S,
    LEADS_CHARS as LEADS_CHARS,
    LEADS_HEADER as LEADS_HEADER,
    LEDGER_LINE_CHARS as LEDGER_LINE_CHARS,
    LEVEL_ORDER as LEVEL_ORDER,
    MAX_BLAME_LINES as MAX_BLAME_LINES,
    MAX_CITATIONS as MAX_CITATIONS,
    MAX_COUNT_HITS as MAX_COUNT_HITS,
    MAX_DEEP_FINDINGS as MAX_DEEP_FINDINGS,
    MAX_FILE_BYTES as MAX_FILE_BYTES,
    MAX_GREP_HITS as MAX_GREP_HITS,
    MAX_HISTORY as MAX_HISTORY,
    MAX_LEADS as MAX_LEADS,
    MAX_LIKE as MAX_LIKE,
    MAX_LIKE_SCANNED as MAX_LIKE_SCANNED,
    MAX_LIST as MAX_LIST,
    MAX_NOTES as MAX_NOTES,
    MAX_PATTERN_CHARS as MAX_PATTERN_CHARS,
    MAX_READ_LINES as MAX_READ_LINES,
    MAX_RECORDED_QUERIES as MAX_RECORDED_QUERIES,
    MAX_REFERENCES as MAX_REFERENCES,
    MAX_REQUESTS as MAX_REQUESTS,
    MAX_ROUNDS as MAX_ROUNDS,
    MERGE_WINDOW as MERGE_WINDOW,
    MIN_LOOKUPS as MIN_LOOKUPS,
    NOTE_CHARS as NOTE_CHARS,
    ROUND_CHARS as ROUND_CHARS,
    SEVERITY_BY_LEVEL,
    TRIAGE_CONTRACT as TRIAGE_CONTRACT,
    Rejected as Rejected,
    Repo as Repo,
    _CITATION as _CITATION,
    _GLOB as _GLOB,
    _NON_CODE as _NON_CODE,
    _SYMBOL as _SYMBOL,
    _add as _add,
    _definition_pattern as _definition_pattern,
    _flat as _flat,
    _hits as _hits,
    _int as _int,
    _list as _list,
    _loose as _loose,
    _numbered as _numbered,
    is_code as is_code,
)
from autoreview_deep_serve import (
    _PROBE_RUNNER as _PROBE_RUNNER,
    _serve_graph as _serve_graph,
    _serve_notes as _serve_notes,
    _serve_probe as _serve_probe,
    _serve_rules as _serve_rules,
    serve as serve,
    serve_blocks as serve_blocks,
    serve_request as serve_request,
)
from infer_api import FrameError as FrameError, Incomplete as Incomplete
from knowledge_api import (
    GRAPH_SCHEMA as GRAPH_SCHEMA,
    Knowledge as Knowledge,
    QueryError as QueryError,
    construct_label as construct_label,
)
from review_api import (
    _CALL as _CALL,
    _COMMON as _COMMON,
    _DEF as _DEF,
    _TEST as _TEST,
    FileDiff,
    Runner,
    _clip,
    _run_process as _run_process,
    _text,
    file_kind,
    normalize_category as normalize_category,
    skip_reason,
)
from review_prompts import (
    DEEP_LENSES,
    DEEP_NUDGE as DEEP_NUDGE,
    DEEP_SHAPE_NUDGE as DEEP_SHAPE_NUDGE,
    DEEP_SHAPES_HEADER as DEEP_SHAPES_HEADER,
    DEEP_TRACE_NUDGE as DEEP_TRACE_NUDGE,
    RISK_TRIAGE_TASK,
    deep_task as deep_task,
    risk_tracer_task,
)

LEVEL_TO_OLD = SEVERITY_BY_LEVEL

__all__ = [
    "reference_module",
    "testing",
    "Repo",
    "Rejected",
    "Brief",
    "LEVEL_TO_OLD",
    "serve_request",
    "serve_blocks",
    "serve",
    "extract",
    "build_brief",
    "merge_rules",
    "check_citations",
    "normalize_deep",
    "cited_windows",
    "merge",
    "shapes_by_lens",
    "investigate",
    "prove_unpinned",
    "regression_findings",
    "index_diff",
    "diff_view",
    "shared_views",
    "no_tests",
    "start_tests",
    "tests_block",
    "run_deep",
    "refuted_leads",
    "assurance",
    "is_code",
]


def index_diff(diff_text: str) -> dict[str, str]:
    """Split rendered diff text into {path: section} keyed by `File: <path>` headers."""
    sections: dict[str, str] = {}
    matches = list(re.finditer(r"^File: (\S+)", diff_text, re.M))
    for index, match in enumerate(matches):
        end = matches[index + 1].start() if index + 1 < len(matches) else len(diff_text)
        sections[match.group(1)] = diff_text[match.start() : end].rstrip()
    return sections


def _file_manifest(files: list[FileDiff], limit: int = 6_000) -> str:
    """Compact per-area manifest of changed non-test files so triage sees every service."""
    by_area: dict[str, list[tuple[int, str, int, int]]] = {}
    test_count = 0
    for item in files:
        if skip_reason(item):
            continue
        if file_kind(item.path) == "test":
            test_count += 1
            continue
        area = item.path.split("/", 1)[0] + "/" if "/" in item.path else "(root)"
        by_area.setdefault(area, []).append(
            (item.added + item.removed, item.path, item.added, item.removed)
        )
    if not by_area:
        return ""

    header = (
        'Changed production/doc files by area '
        '(use {"read_diff": {"path": "..."}} for any file past the diff cut):'
    )
    lines = [header]
    per_area = max(6, 40 // max(1, len(by_area)))
    for area in sorted(by_area):
        entries = sorted(by_area[area], key=lambda row: (-row[0], row[1]))
        lines.append(f"[{area}] ({len(entries)} files)")
        for _, path, added, removed in entries[:per_area]:
            lines.append(f"  {path} (+{added}/-{removed})")
        if len(entries) > per_area:
            lines.append(f"  (+{len(entries) - per_area} more in {area})")
    if test_count:
        lines.append(f"(+{test_count} changed test file(s))")
    return _clip("\n".join(lines), limit)


def _order_diff(diff_text: str) -> str:
    """Order production code and docs before tests so large test diffs do not hide code."""
    matches = list(re.finditer(r"^File: (\S+)", diff_text, re.M))
    if len(matches) < 2:
        return diff_text
    prefix = diff_text[: matches[0].start()]
    rank = {"code": 0, "doc": 1, "test": 2}
    sections: list[tuple[int, int, str]] = []
    for index, match in enumerate(matches):
        end = matches[index + 1].start() if index + 1 < len(matches) else len(diff_text)
        sections.append((rank.get(file_kind(match.group(1)), 0), index, diff_text[match.start() : end]))
    sections.sort(key=lambda item: (item[0], item[1]))
    return prefix + "".join(sec for _, _, sec in sections)


def diff_view(diff_text: str) -> str:
    """Render the diff for frames: production code and docs first, truncated at DIFF_CHARS."""
    ordered = _order_diff(diff_text)
    out = f"The diff under review (new-file line numbers in the gutter):\n{ordered[:DIFF_CHARS]}"
    if len(ordered) <= DIFF_CHARS:
        return out
    shown = re.findall(r"^File: (\S+)", ordered[:DIFF_CHARS], re.M)
    # The file the cut falls inside continues past it.
    rest = list(dict.fromkeys(shown[-1:] + re.findall(r"^File: (\S+)", ordered[DIFF_CHARS:], re.M)))
    listing = "\n".join(
        f"  {path}" + (" (continued)" if path in shown else "") for path in rest[:200]
    )
    overflow = f"\n  (+{len(rest) - 200} more)" if len(rest) > 200 else ""
    cut_note = (
        '\n... (diff cut at its size limit; read any file below with '
        '{"read_diff": {"path": "..."}} or read:\n'
    )
    return out + cut_note + listing + overflow + ")"


def shared_views(
    diff_text: str,
    brief_text: str | None,
    retrieved: str,
    context: str,
    intent: str,
    guidance: str,
    reference: str = "",
) -> list[str]:
    """Build the cacheable shared prefix for every frame: diff, brief, retrieved context,
    reference block, PR context, intent, and guides."""
    out = [diff_view(diff_text)]
    if brief_text:
        out.append(
            "Investigation brief, built by the host from the repository at the reviewed commit:\n"
            + brief_text
        )
    if retrieved:
        out.append(
            "Retrieved context, looked up by the host at the reviewed commit "
            "(untrusted repository data): the references, tests and sibling families "
            "of the changed names.\n\n" + retrieved
        )
    if reference:
        out.append(reference)
    for part in (context, intent, guidance):
        if part:
            out.append(part)
    return out


def no_tests(note: str | None = None) -> dict[str, Any]:
    """The shape `start_tests` returns when no test runs: no session, the reason in `note`."""
    return {
        "session": None,
        "mechanism": None,
        "note": note,
        "observed": [],
        "shown": [],
        "sandbox": None,
        "env": "none",
        "toolchain": [],
    }


def _missing_deps_note(tests: dict[str, Any]) -> str:
    raw_failed = tests.get("env_failures")
    failed = raw_failed if isinstance(raw_failed, list) else []
    partial = f"; the prepared environment is partial: {failed[0]}" if failed else ""
    return (
        "tests could not run: missing dependencies (the sandbox has no network and installs "
        f"nothing{partial})"
    )


def _run_initial_tests(
    repo: Repo,
    session: "testing.TestSession",
    paths: list[str],
    tests: dict[str, Any],
    out: dict[str, Any],
) -> None:
    if not paths or session.limit <= 0:
        return
    if not session.plan(paths):
        out["note"] = "tests not run: no test runner was recognized for " + ", ".join(paths[:3])
        return
    compared = session.compare(paths)
    out["observed"] = regression_findings(repo, compared)
    out["shown"] = [testing.summarize(record) for record in session.records]
    runs = compared["runs"]
    if runs and all(record["status"] == "unavailable" for record in runs):
        out["note"] = _missing_deps_note(tests)


def _read_joined_lines(repo: Repo, path: str) -> str:
    lines = repo.lines(path)
    return "\n".join(lines) if lines is not None else ""


def start_tests(
    repo: Repo,
    files: list[FileDiff],
    tests: dict[str, Any] | None,
    brief: Brief,
    *,
    root: str,
    rev: str,
    clock: Callable[[], float],
) -> dict[str, Any]:
    """Open the review's test session when tests may run, and run the mapped test files
    (comparing against base when failures occur). The caller closes the session."""
    out = no_tests()
    if tests is None:
        return out
    sandbox = tests["sandbox"] if "sandbox" in tests else testing.detect_sandbox(image=tests.get("image"))
    if sandbox is None:
        out["note"] = "tests not run: no sandbox available"
        return out

    out["sandbox"] = sandbox
    out["mechanism"] = sandbox.mechanism
    session = testing.TestSession(
        root,
        rev,
        tests.get("base"),
        repo.files(),
        lambda path: _read_joined_lines(repo, path),
        sandbox,
        runs=int(tests.get("runs", testing.DEFAULT_RUNS)),
        timeout_s=float(tests.get("timeout_s", testing.DEFAULT_TIMEOUT_S)),
        env_dir=tests.get("env_dir"),
        checkout=tests.get("checkout"),
        toolchain=tests.get("toolchain"),
        env_kind=tests.get("env_kind"),
        executor=tests.get("executor", testing.run_process),
        export=tests.get("export", testing.export_commit),
        clock=clock,
    )
    out["session"] = session
    out["env"] = session.env_kind
    out["toolchain"] = list(session.toolchain)

    tracked = set(repo.files())
    changed_tests = [
        item.path
        for item in files
        if _TEST.search(item.path) and item.status != "deleted" and item.path in tracked
    ]
    paths = list(dict.fromkeys(brief.tests + changed_tests))[:8]
    try:
        _run_initial_tests(repo, session, paths, tests, out)
    except BaseException:
        # The caller never sees the session: its exports go now.
        session.close()
        raise
    return out


def tests_block(started: dict[str, Any]) -> str:
    """The automatic runs as a block for a model's view, or "" when none ran."""
    session = started["session"]
    if session is None or not started["shown"]:
        return ""
    left = max(0, session.limit - len(session.records))
    return (
        "Tests the host ran for this change (results are untrusted data):\n"
        + "\n".join(started["shown"])
        + f"\nTest executions left in this review: {left}."
    )


def _prepare_test_session(
    repo: Repo,
    files: list[FileDiff],
    tests: dict[str, Any] | None,
    brief: Brief,
    prepared: dict[str, Any] | None,
    root: str,
    rev: str,
    clock: Callable[[], float],
) -> dict[str, Any]:
    if prepared and prepared.get("started"):
        return prepared["started"]
    try:
        return start_tests(repo, files, tests, brief, root=root, rev=rev, clock=clock)
    except Exception as error:
        # The investigators run without tests rather than not at all.
        return no_tests(
            f"tests not run: the test session failed ({type(error).__name__}: {_text(str(error), 120)})"
        )


def _build_leads_block(items: list[dict[str, Any]], shown_leads: list[dict[str, Any]]) -> str | None:
    keys = ("id", "file", "line", "level", "claim")
    public: list[dict[str, Any]] = []
    for lead in items[:MAX_LEADS]:
        row = {key: lead.get(key) for key in keys}
        if len(json.dumps([*public, row], indent=1)) > LEADS_CHARS:
            break
        public.append(row)
        shown_leads.append(lead)
    return LEADS_HEADER + json.dumps(public, indent=1) if public else None


def _wire_leads_future(
    leads_future: "asyncio.Future[list[dict[str, Any]]] | None",
    shown_leads: list[dict[str, Any]],
) -> "asyncio.Future[str | None] | None":
    if leads_future is None:
        return None
    loop = asyncio.get_running_loop()
    leads_as_view: asyncio.Future[str | None] = loop.create_future()

    def forward(done: "asyncio.Future[list[dict[str, Any]]]") -> None:
        if leads_as_view.done():
            return
        if done.cancelled() or done.exception() is not None:
            leads_as_view.set_result(None)
            return
        raw_leads = done.result()
        items = list(raw_leads) if raw_leads is not None else []
        leads_as_view.set_result(_build_leads_block(items, shown_leads))

    leads_future.add_done_callback(forward)
    return leads_as_view


def _collect_structural_findings(
    brief: Brief,
    cap: Callable[[str, str, str], str],
    enrich: Callable[[Any, dict[str, Any]], None] | None,
    generic: Callable[[dict[str, Any]], str | None] | None,
    generic_dropped: list[str],
) -> list[dict[str, Any]]:
    if brief.reference is None:
        return []
    structural: list[dict[str, Any]] = []
    try:
        for item in brief.reference.structural_findings():
            item["level"] = cap(item["level"], item["category"], item.get("scenario") or "")
            item["severity"] = SEVERITY_BY_LEVEL[item["level"]]
            if enrich is not None:
                enrich({"unpinned": None, "consequence": ""}, item)
            reason = generic(item) if generic is not None else None
            if reason:
                generic_dropped.append(reason)
                continue
            structural.append(item)
    except Exception:
        return []
    return structural


def _build_risk_tracer_extra(
    item: dict[str, Any],
    repo: Repo,
    scoped_base: list[str] | None,
    shown_in_diff: set[str],
) -> list[str]:
    extra: list[str] = []
    starts = item.get("start_files") if isinstance(item.get("start_files"), list) else []
    for start_file in starts[:4]:
        path = str(start_file).strip().lstrip("./")
        if not path or path not in repo.diff_by_file:
            continue
        snippet = _clip(repo.diff_by_file[path], 16_000)
        if scoped_base is not None:
            extra.append(f"Diff hunks for start file {path}:\n{snippet}")
        elif path not in shown_in_diff:
            extra.append(f"Diff hunks for start file {path} (past the main diff cut):\n{snippet}")
    return extra


def _triage_scoped_base(
    views: list[str], manifest: str, shared_prefix: bool
) -> list[str] | None:
    if shared_prefix or not views:
        return None
    # Without shared_prefix, drop the full 200 KB diff and send only start_files + manifest.
    scoped_base = list(views[1:])
    return [manifest, *scoped_base] if manifest else scoped_base


def _parse_risk_tasks(
    raw_paths: list[Any],
    repo: Repo,
    diff_text: str,
    scoped_base: list[str] | None,
    session: "testing.TestSession | None",
) -> list[tuple[str, str, list[str], list[str] | None]]:
    graph_schema = GRAPH_SCHEMA if repo.graph is not None else ""
    shown_in_diff = set(re.findall(r"^File: (\S+)", _order_diff(diff_text)[:DIFF_CHARS], re.M))
    risk_tasks: list[tuple[str, str, list[str], list[str] | None]] = []
    for index, item in enumerate(raw_paths[:8], 1):
        if not isinstance(item, dict) or not item.get("question"):
            continue
        rid = str(item.get("id") or f"R{index}").strip()
        extra = _build_risk_tracer_extra(item, repo, scoped_base, shown_in_diff)
        task_text = risk_tracer_task(
            item,
            session is not None,
            notes=repo.knowledge is not None,
            graph=graph_schema,
        )
        risk_tasks.append((f"risk:{rid}", task_text, extra, scoped_base))
    return risk_tasks


async def _run_risk_triage(
    frames: Any,
    files: list[FileDiff],
    views: list[str],
    repo: Repo,
    diff_text: str,
    *,
    model: str | None,
    thinking: str | None,
    cutoff: float | None,
    shared: list[str],
    shared_prefix: bool,
    session: "testing.TestSession | None",
) -> list[tuple[str, str, list[str], list[str] | None]]:
    manifest = _file_manifest(files)
    triage_views = [*views, manifest] if manifest else views
    triage_results = await frames.run(
        "deep",
        [("triage", RISK_TRIAGE_TASK, triage_views)],
        contract=TRIAGE_CONTRACT,
        model=model,
        thinking=thinking,
        cutoff=cutoff,
        context=shared if shared_prefix else None,
    )
    triage_res = triage_results[0] if triage_results else None
    raw_paths = triage_res.get("paths") if isinstance(triage_res, dict) else None
    if not isinstance(raw_paths, list):
        return []
    scoped_base = _triage_scoped_base(views, manifest, shared_prefix)
    return _parse_risk_tasks(raw_paths, repo, diff_text, scoped_base, session)


@dataclass
class _DeepRunContext:
    frames: Any
    repo: Repo
    brief: Brief
    views: list[str]
    shared: list[str]
    shared_prefix: bool
    rounds: int
    model: str | None
    thinking: str | None
    cutoff: float | None
    clock: Callable[[], float]
    session: "testing.TestSession | None"
    outside: bool
    owed: dict[str, list[dict[str, str]]]
    leads_as_view: "asyncio.Future[str | None] | None"
    diff_lines: dict[str, set[int]]
    cap: Callable[[str, str, str], str]
    to_level: Callable[[Any], str | None] | None
    enrich: Callable[[Any, dict[str, Any]], None] | None
    generic: Callable[[dict[str, Any]], str | None] | None
    on_investigator: Callable[[str, list[dict[str, Any]]], Any] | None
    findings: list[dict[str, Any]]
    dropped: list[str]
    generic_dropped: list[str]
    kept_by_lens: dict[str, int]


def _normalize_investigator_findings(
    ctx: _DeepRunContext, lens: str, raw: list[Any]
) -> list[dict[str, Any]]:
    mine: list[dict[str, Any]] = []
    session_runs = list(ctx.session.records) if ctx.session is not None else []
    for item in raw[:MAX_DEEP_FINDINGS]:
        finding, problem = normalize_deep(
            ctx.repo, item, lens, ctx.diff_lines, ctx.cap, session_runs, ctx.to_level, ctx.enrich
        )
        if finding is None:
            ctx.dropped.append(f"{lens}: {problem}")
            continue
        reason = ctx.generic(finding) if ctx.generic is not None else None
        if reason:
            ctx.generic_dropped.append(reason)
            continue
        ctx.kept_by_lens[lens] = ctx.kept_by_lens.get(lens, 0) + 1
        mine.append(finding)
    return mine


async def _notify_investigator(
    callback: Callable[[str, list[dict[str, Any]]], Any] | None,
    lens: str,
    findings: list[dict[str, Any]],
) -> None:
    if callback is None or not findings:
        return
    try:
        await callback(lens, findings)
    except Exception:
        # Early verification is an optimisation: the final stage verifies what it missed.
        return


async def _run_one_investigator(
    ctx: _DeepRunContext,
    lens: str,
    task_override: str | None = None,
    extra_views: list[str] | None = None,
    base_views: list[str] | None = None,
) -> dict[str, Any]:
    base = ctx.views if base_views is None else base_views
    lens_views = [*base, *extra_views] if extra_views else base
    shielded_leads = asyncio.shield(ctx.leads_as_view) if ctx.leads_as_view is not None else None
    req = None if task_override else ctx.brief.required.get(lens)
    lens_shapes = None if task_override else ctx.owed.get(lens)
    shared_ctx = ctx.shared if ctx.shared_prefix else None

    outcome = await investigate(
        ctx.frames, lens, lens_views, ctx.repo,
        rounds=ctx.rounds, model=ctx.model, thinking=ctx.thinking,
        cutoff=ctx.cutoff, clock=ctx.clock, tests=ctx.session,
        something_outside=ctx.outside, required=req, shapes=lens_shapes,
        shared=shared_ctx, leads_view=shielded_leads, task_override=task_override,
    )
    reply = outcome["reply"]
    raw = reply.get("findings") if isinstance(reply.get("findings"), list) else []
    mine = _normalize_investigator_findings(ctx, lens, raw)
    ctx.findings.extend(mine)
    await _notify_investigator(ctx.on_investigator, lens, mine)
    return outcome


def _resolve_future_leads(
    leads_future: "asyncio.Future[list[dict[str, Any]]] | None",
    fallback: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    if leads_future is None:
        return fallback
    ok = leads_future.done() and not leads_future.cancelled() and leads_future.exception() is None
    if not ok:
        return []
    result = leads_future.result()
    return list(result) if result is not None else []


def _collect_checked_assurance(
    outcomes: list[dict[str, Any]], kept_by_lens: dict[str, int]
) -> list[str]:
    checked: list[str] = []
    # What the claims investigator verified leads the assurance.
    for outcome in sorted(outcomes, key=lambda item: item["lens"] != "claims"):
        reply, record = outcome["reply"], outcome["record"]
        raw_checked = reply.get("checked") if isinstance(reply.get("checked"), list) else []
        for sentence in raw_checked:
            text = _text(sentence, 1000)
            # Declarations of owed checks ("T2: not applicable, ...") are accounting, not assurance.
            if re.match(r"^\s*T\d+\b", text) or "not applicable" in text.lower():
                continue
            if text and record["status"] != "failed" and len(checked) < 3:
                checked.append(text if text.endswith((".", "!", "?", "…")) else text + ".")
        record["findings"] = kept_by_lens.get(outcome["lens"], 0)
    return checked


def _build_diff_lines(files: list[FileDiff]) -> dict[str, set[int]]:
    return {
        item.path: {line.new for hunk in item.hunks for line in hunk.lines if line.new is not None}
        for item in files
    }


def _build_shared_context(
    diff_text: str,
    brief: Brief,
    started: dict[str, Any],
    prepared: dict[str, Any] | None,
    context: str,
    intent: str,
    guidance: str,
    reference_view: str,
) -> list[str]:
    block = tests_block(started)
    brief_text = f"{brief.text}\n\n{block}" if block else brief.text
    retrieved = prepared["retrieved"] if prepared and prepared.get("retrieved") else ""
    ref_text = (brief.reference_text or "") if reference_view in ("all", "investigators") else ""
    return shared_views(
        diff_text, brief_text, retrieved, context, intent, guidance, reference=ref_text
    )


async def _gather_outcomes(
    ctx: _DeepRunContext,
    lenses: list[str],
    risk_tasks: list[tuple[str, str, list[str], list[str] | None]],
) -> list[dict[str, Any]]:
    if risk_tasks:
        coros = [
            _run_one_investigator(ctx, lens, task_override=task, extra_views=extra, base_views=base)
            for lens, task, extra, base in risk_tasks
        ]
        return list(await asyncio.gather(*coros))
    return list(await asyncio.gather(*(_run_one_investigator(ctx, lens) for lens in lenses)))


def _format_public_runs(runs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {key: value for key, value in record.items() if key != "output"}
        | {"output": record["output"][-600:]}
        for record in runs
    ]


def _setup_repo_and_brief(
    prepared: dict[str, Any] | None,
    root: str,
    rev: str,
    runner: Runner | None,
    diff_text: str,
    files: list[FileDiff],
    read_file: Callable[[str], list[str] | None],
    title: str,
    description: str,
    base: str | None,
) -> tuple[Repo, Brief]:
    repo = prepared["repo"] if prepared else Repo(root, rev, runner)
    if not repo.files():
        raise RuntimeError("the reviewed commit could not be read")
    repo.diff_by_file = index_diff(diff_text)
    brief = (
        prepared["brief"]
        if prepared
        else build_brief(repo, files, read_file, title=title, description=description, base=base)
    )
    return repo, brief


def _select_active_lenses(brief: Brief, only: list[str] | None) -> list[str]:
    return [lens for lens in brief.lenses if lens in DEEP_LENSES and (not only or lens in only)]


def _has_outside_context(brief: Brief, repo: Repo, files: list[FileDiff]) -> bool:
    return bool(brief.callers or brief.tests or brief.helpers or len(repo.files()) > len(files))


def _prepare_initial_views(
    shared: list[str],
    shared_prefix: bool,
    leads: list[dict[str, Any]],
    leads_future: "asyncio.Future[list[dict[str, Any]]] | None",
    shown_leads: list[dict[str, Any]],
) -> tuple[list[str], "asyncio.Future[str | None] | None"]:
    views: list[str] = [] if shared_prefix else list(shared)
    lead_view = _build_leads_block(leads, shown_leads)
    if lead_view and leads_future is None:
        views.append(lead_view)
    leads_as_view = _wire_leads_future(leads_future, shown_leads)
    return views, leads_as_view


def _prove_and_collect_runs(
    session: "testing.TestSession | None",
    repo: Repo,
    findings: list[dict[str, Any]],
    leads_future: "asyncio.Future[list[dict[str, Any]]] | None",
    leads: list[dict[str, Any]],
    prove_leads: bool,
    brief: Brief,
) -> list[dict[str, Any]]:
    resolved_leads = _resolve_future_leads(leads_future, leads)
    to_prove = [*findings, *resolved_leads] if prove_leads else list(findings)
    prove_unpinned(session, repo, to_prove, brief)
    return list(session.records) if session is not None else []


async def run_deep(
    frames: Any, files: list[FileDiff], read_file: Callable[[str], list[str] | None], *,
    reference_view: str = "investigators", root: str, rev: str, diff_text: str,
    leads: list[dict[str, Any]], context: str, rounds: int, model: str | None,
    thinking: str | None, cutoff: float | None, clock: Callable[[], float],
    cap: Callable[[str, str, str], str], runner: Runner | None = None,
    only: list[str] | None = None, tests: dict[str, Any] | None = None,
    to_level: Callable[[Any], str | None] | None = None, title: str = "",
    description: str = "", base: str | None = None, guidance: str = "",
    enrich: Callable[[Any, dict[str, Any]], None] | None = None,
    generic: Callable[[dict[str, Any]], str | None] | None = None, intent: str = "",
    prepared: dict[str, Any] | None = None, prove_leads: bool = True,
    keep_session: bool = False, shapes: list[dict[str, str]] | None = None,
    shared_prefix: bool = False,
    leads_future: "asyncio.Future[list[dict[str, Any]]] | None" = None,
    on_investigator: Callable[[str, list[dict[str, Any]]], Any] | None = None,
    risk_triage: bool = False,
) -> dict[str, Any]:
    """Run the deep pass: map, optional sandboxed tests, investigators, and evidence checks.
    Returns findings, dropped reasons, investigator records, test runs, and brief metadata."""
    repo, brief = _setup_repo_and_brief(
        prepared, root, rev, runner, diff_text, files, read_file, title, description, base
    )
    lenses = _select_active_lenses(brief, only)
    outside = _has_outside_context(brief, repo, files)
    diff_lines = _build_diff_lines(files)

    session: testing.TestSession | None = None
    try:
        started = _prepare_test_session(repo, files, tests, brief, prepared, root, rev, clock)
        session = started["session"]
        shared = _build_shared_context(
            diff_text, brief, started, prepared, context, intent, guidance, reference_view
        )
        shown_leads: list[dict[str, Any]] = []
        views, leads_as_view = _prepare_initial_views(
            shared, shared_prefix, leads, leads_future, shown_leads
        )

        findings: list[dict[str, Any]] = list(started["observed"])
        dropped: list[str] = []
        generic_dropped: list[str] = []
        structural = _collect_structural_findings(brief, cap, enrich, generic, generic_dropped)
        findings.extend(structural)
        await _notify_investigator(on_investigator, "structure", structural)

        shape_list = list(shapes) if shapes is not None else []
        ctx = _DeepRunContext(
            frames=frames, repo=repo, brief=brief, views=views, shared=shared,
            shared_prefix=shared_prefix, rounds=max(1, min(MAX_ROUNDS, rounds)),
            model=model, thinking=thinking, cutoff=cutoff, clock=clock, session=session,
            outside=outside, owed=shapes_by_lens(shape_list, lenses),
            leads_as_view=leads_as_view, diff_lines=diff_lines, cap=cap,
            to_level=to_level, enrich=enrich, generic=generic,
            on_investigator=on_investigator, findings=findings, dropped=dropped,
            generic_dropped=generic_dropped, kept_by_lens={},
        )
        risk_tasks: list[tuple[str, str, list[str], list[str] | None]] = []
        if risk_triage and not only:
            risk_tasks = await _run_risk_triage(
                frames, files, views, repo, diff_text, model=model, thinking=thinking,
                cutoff=cutoff, shared=shared, shared_prefix=shared_prefix, session=session,
            )
        outcomes = await _gather_outcomes(ctx, lenses, risk_tasks)
        runs = _prove_and_collect_runs(
            session, repo, findings, leads_future, leads, prove_leads, brief
        )
    finally:
        # The exports are removed whatever happened, unless the caller keeps the session.
        if session is not None and not keep_session:
            session.close()

    refuted = refuted_leads(repo, outcomes, shown_leads, diff_lines)
    checked = _collect_checked_assurance(outcomes, ctx.kept_by_lens)
    return {
        "findings": findings,
        "dropped": dropped,
        "generic": generic_dropped,
        "investigators": [outcome["record"] for outcome in outcomes],
        "checked": checked,
        "refuted": refuted,
        "repo": repo,
        "brief": brief,
        "lenses": lenses,
        "diff_cut": len(diff_text) > DIFF_CHARS,
        "tests": {
            "enabled": tests is not None,
            "mechanism": started["mechanism"],
            "note": started["note"],
            "runs": _format_public_runs(runs),
            "env": started["env"],
            "toolchain": started["toolchain"],
        },
    }
