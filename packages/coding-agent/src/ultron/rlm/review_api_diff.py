"""Argument parsing, diff parsing, chunk building, and scope resolution for `review_api`."""
from __future__ import annotations

import json
import os
import re
import shlex
import shutil
from dataclasses import dataclass
from functools import partial
from pathlib import Path
from typing import Any, Callable

from review_prompts import ALIASES, REVIEWERS, Reviewer, finder_task
from review_api_types import (
    ACCEPTANCE_KEY,
    CHUNK_CHARS,
    CONTEXT_LINES,
    CONTRACT_KEY,
    CONVENTIONS_KEY,
    DEEP_DEFAULT,
    DEEP_MAX,
    EMPTY_TREE,
    GH_TIMEOUT_S,
    LESSONS_KEY,
    MAX_UNTRACKED_BYTES,
    MAX_UNTRACKED_FILES,
    REUSE_KEY,
    USAGE,
    Chunk,
    DiffLine,
    FileDiff,
    Git,
    Hunk,
    Options,
    ReviewError,
    Runner,
    Scope,
    Target,
    _DIFF_GIT,
    _HUNK,
    _LLM_HINT,
    _OPS_DOC,
    _PR_URL,
    _PREFIX,
    _context,
    _estimate_tokens,
    _local_reader,
    _rev_reader,
    _run_process,
    _split_hunk,
    _tokens,
    _unquote,
    _within,
    added_file,
    default_branch,
    file_kind,
    render_hunk,
    skip_reason,
)


def _reviewer_keys(value: str) -> list[str]:
    from review_shared import parse_reviewer_keys

    return parse_reviewer_keys(value, reviewers=REVIEWERS, aliases=ALIASES, error_cls=ReviewError, require_non_empty=True)


def _tokenize_args(text: str | list[str] | None) -> list[str]:
    if isinstance(text, list):
        return list(text)
    try:
        return shlex.split(text or "")
    except ValueError as error:
        raise ReviewError(f"cannot parse the arguments: {error}") from None


def _apply_env_defaults(options: Options, tokens: list[str], env: dict[str, str]) -> list[str]:
    budget_env = env.get("ULTRON_REVIEW_BUDGET", "").strip()
    if budget_env:
        options.budget_tokens = _tokens(budget_env)
    only_env = env.get("ULTRON_REVIEW_ONLY", "").strip()
    if only_env:
        options.only = _reviewer_keys(only_env)
    model_env = env.get("ULTRON_REVIEW_MODEL", "").strip()
    if model_env:
        return ["--model", model_env, *tokens]
    return tokens


def _next_flag_value(tokens: list[str], index: int, flag: str, inline: str | None) -> tuple[str, int]:
    if inline is not None:
        return inline, index
    next_index = index + 1
    if next_index >= len(tokens):
        raise ReviewError(f"{flag} needs a value")
    return tokens[next_index], next_index


def _parse_deep_option(inline: str | None) -> int:
    if inline is None:
        return DEEP_DEFAULT
    if inline.isdigit() and int(inline) <= DEEP_MAX:
        return int(inline)
    raise ReviewError(f"--deep=N takes a number from 0 to {DEEP_MAX}")


def _parse_model_option(raw_value: str) -> str:
    model = raw_value.strip()
    if not re.fullmatch(r"[^/\s]+/[^/\s]+(?:/[^/\s]+)*", model):
        raise ReviewError(f"--model takes provider/model, not {model!r}")
    return model


def _apply_value_flag(options: Options, tokens: list[str], index: int, flag: str, inline: str | None) -> int:
    if flag == "--only":
        raw, next_index = _next_flag_value(tokens, index, flag, inline)
        options.only = _reviewer_keys(raw)
        return next_index
    if flag == "--budget":
        raw, next_index = _next_flag_value(tokens, index, flag, inline)
        options.budget_tokens = _tokens(raw)
        return next_index
    if flag == "--model":
        raw, next_index = _next_flag_value(tokens, index, flag, inline)
        options.model = _parse_model_option(raw)
        return next_index
    raise ReviewError(f"unknown option {tokens[index]}\n\n{USAGE}")


def _consume_arg_token(options: Options, tokens: list[str], index: int) -> int:
    token = tokens[index]
    if not token.startswith("-") or token == "-":
        options.targets.append(token)
        return index
    flag, sep, inline_value = token.partition("=")
    inline = inline_value if sep else None
    if flag in ("-h", "--help"):
        options.help = True
        return index
    if flag == "--post":
        options.post = True
        return index
    if flag in ("--plan", "--dry-run"):
        options.plan = True
        return index
    if flag == "--deep":
        options.deep = _parse_deep_option(inline)
        return index
    return _apply_value_flag(options, tokens, index, flag, inline)


def parse_args(text: str | list[str] | None, env: dict[str, str] | None = None) -> Options:
    """Parse `/review`'s argument string (shell-style quoting). `env` supplies defaults for the flags:
    ULTRON_REVIEW_BUDGET, ULTRON_REVIEW_MODEL and ULTRON_REVIEW_ONLY."""
    options = Options()
    tokens = _apply_env_defaults(options, _tokenize_args(text), env or {})
    index = 0
    while index < len(tokens):
        if tokens[index] == "--":
            options.explicit_paths.extend(tokens[index + 1:])
            break
        index = _consume_arg_token(options, tokens, index) + 1
    return options


def _assign_pr(target: Target, pr: int) -> None:
    if target.pr is not None and target.pr != pr:
        raise ReviewError("give at most one pull request")
    target.pr = pr


def _classify_target_token(
    target: Target,
    token: str,
    *,
    exists: Callable[[str], bool],
    is_ref: Callable[[str], bool],
) -> None:
    number = re.fullmatch(r"#?(\d+)", token)
    url = _PR_URL.match(token)
    if url:
        _assign_pr(target, int(url.group(1)))
        return
    if number and not exists(token):
        _assign_pr(target, int(number.group(1)))
        return
    if exists(token):
        target.paths.append(token)
        return
    if not is_ref(token):
        raise ReviewError(f"{token!r} is not a path in this repository, a git ref, or a PR number")
    if target.base is not None:
        raise ReviewError(f"give at most one base ref (got {target.base!r} and {token!r})")
    target.base = token


def classify_targets(options: Options, *, exists: Callable[[str], bool], is_ref: Callable[[str], bool]) -> Target:
    """Sort positional arguments into at most one PR or base ref, and paths."""
    target = Target(paths=list(options.explicit_paths))
    for token in options.targets:
        _classify_target_token(target, token, exists=exists, is_ref=is_ref)
    if target.pr is not None and target.base is not None:
        raise ReviewError("give a PR number or a base ref, not both")
    return target


@dataclass
class _DiffCursor:
    current: FileDiff | None = None
    hunk: Hunk | None = None
    old_left: int = 0
    new_left: int = 0
    old_no: int = 0
    new_no: int = 0


def _consume_hunk_line(cursor: _DiffCursor, raw: str) -> bool:
    if cursor.hunk is None or (cursor.old_left <= 0 and cursor.new_left <= 0):
        return False
    marker, body = (raw[:1], raw[1:]) if raw else (" ", "")
    if marker == "\\":
        return True
    if marker == "+":
        cursor.hunk.lines.append(DiffLine("+", None, cursor.new_no, body))
        cursor.new_no += 1
        cursor.new_left -= 1
        return True
    if marker == "-":
        cursor.hunk.lines.append(DiffLine("-", cursor.old_no, None, body))
        cursor.old_no += 1
        cursor.old_left -= 1
        return True
    if marker == " ":
        cursor.hunk.lines.append(DiffLine(" ", cursor.old_no, cursor.new_no, body))
        cursor.old_no += 1
        cursor.new_no += 1
        cursor.old_left -= 1
        cursor.new_left -= 1
        return True
    cursor.hunk = None
    return False


def _apply_diff_meta_header(current: FileDiff, raw: str) -> bool:
    if raw.startswith("new file mode"):
        current.status = "added"
        current.old_path = None
        return True
    if raw.startswith("deleted file mode"):
        current.status = "deleted"
        return True
    if raw.startswith("rename from "):
        current.old_path = _unquote(raw[len("rename from "):])
        current.status = "renamed"
        return True
    if raw.startswith("rename to "):
        current.path = _unquote(raw[len("rename to "):])
        current.status = "renamed"
        return True
    if raw.startswith("Binary files ") or raw.startswith("GIT binary patch"):
        current.binary = True
        return True
    return False


def _apply_diff_path_header(current: FileDiff, raw: str) -> bool:
    if raw.startswith("--- "):
        source = _unquote(raw[4:].split("\t")[0])
        if source != "/dev/null" and _PREFIX.match(source) and current.status != "added":
            current.old_path = source[2:]
        return True
    if raw.startswith("+++ "):
        dest = _unquote(raw[4:].split("\t")[0])
        if dest == "/dev/null":
            current.status = "deleted"
        elif _PREFIX.match(dest):
            current.path = dest[2:]
        return True
    return False


def _start_hunk(cursor: _DiffCursor, raw: str) -> None:
    match = _HUNK.match(raw)
    if not match or cursor.current is None:
        return
    old_start, old_len = int(match.group(1)), int(match.group(2) or "1")
    new_start, new_len = int(match.group(3)), int(match.group(4) or "1")
    hunk = Hunk(old_start, old_len, new_start, new_len, match.group(5).strip())
    cursor.current.hunks.append(hunk)
    cursor.hunk = hunk
    cursor.old_left, cursor.new_left = old_len, new_len
    cursor.old_no, cursor.new_no = old_start, new_start


def _process_diff_line(files: list[FileDiff], cursor: _DiffCursor, raw: str) -> None:
    if _consume_hunk_line(cursor, raw):
        return
    if raw.startswith("diff --git "):
        match = _DIFF_GIT.match(raw)
        old, new = (match.group(1), match.group(2)) if match else (None, raw.split(" b/")[-1])
        cursor.current = FileDiff(path=new, old_path=old)
        files.append(cursor.current)
        cursor.hunk = None
        return
    if cursor.current is None:
        return
    if _apply_diff_meta_header(cursor.current, raw) or _apply_diff_path_header(cursor.current, raw):
        return
    _start_hunk(cursor, raw)


def parse_diff(text: str) -> list[FileDiff]:
    """Parse unified git diff text (from `git diff` or `gh pr diff`) into files and hunks."""
    files: list[FileDiff] = []
    cursor = _DiffCursor()
    for raw in text.splitlines():
        _process_diff_line(files, cursor, raw)
    for item in files:
        if item.status == "modified" and item.old_path and item.old_path != item.path:
            item.status = "renamed"
    return files


def _file_hunk_blocks(
    item: FileDiff,
    source: list[str] | None,
    max_chars: int,
    context: int,
) -> list[tuple[Hunk, str]]:
    max_lines = max(20, max_chars // 120)
    hunks: list[Hunk] = []
    for hunk in item.hunks:
        if len(hunk.lines) <= max_lines:
            hunks.append(hunk)
        else:
            hunks.extend(_split_hunk(hunk, max_lines))
    blocks: list[tuple[Hunk, str]] = []
    for position, hunk in enumerate(hunks):
        before_floor = hunks[position - 1].new_last + 1 if position > 0 else 1
        after_ceiling = hunks[position + 1].new_first - 1 if position + 1 < len(hunks) else hunk.new_last + context
        lines = [f"@@ {hunk.header}".rstrip() if hunk.header else "@@"]
        lines += _context(source, max(before_floor, hunk.new_first - context), hunk.new_first - 1)
        lines += render_hunk(hunk)
        lines += _context(source, hunk.new_last + 1, min(after_ceiling, hunk.new_last + context))
        blocks.append((hunk, "\n".join(lines)))
    return blocks


def _pack_hunk_groups(blocks: list[tuple[Hunk, str]], max_chars: int) -> list[list[tuple[Hunk, str]]]:
    groups: list[list[tuple[Hunk, str]]] = [[]]
    size = 0
    for block in blocks:
        if groups[-1] and size + len(block[1]) > max_chars:
            groups.append([])
            size = 0
        groups[-1].append(block)
        size += len(block[1])
    return groups


def _chunk_header(
    item: FileDiff,
    part: int,
    total_parts: int,
    listing: str,
    source: list[str] | None,
) -> list[str]:
    label = f"{item.status}" + (f", part {part} of {total_parts}" if total_parts > 1 else "")
    header = [f"File: {item.path} ({label})"]
    if item.old_path and item.status == "renamed":
        header.append(f"Renamed from: {item.old_path}")
    if listing:
        header.append(f"Other files changed in this review: {listing}")
    if source is None and item.status != "added":
        header.append("Surrounding source was not available; only the diff is shown.")
    return header


def _render_file_chunks(
    item: FileDiff,
    source: list[str] | None,
    groups: list[list[tuple[Hunk, str]]],
    listing: str,
    start_id: int,
) -> list[Chunk]:
    rendered: list[Chunk] = []
    for part, group in enumerate(groups, 1):
        header = _chunk_header(item, part, len(groups), listing, source)
        text = "\n".join(header) + "\n\n" + "\n\n".join(block for _, block in group)
        rendered.append(
            Chunk(
                start_id + len(rendered),
                item.path,
                file_kind(item.path),
                item.status,
                [hunk for hunk, _ in group],
                text,
                part,
                len(groups),
            )
        )
    return rendered


def build_chunks(
    files: list[FileDiff],
    read_file: Callable[[str], list[str] | None],
    *,
    max_chars: int = CHUNK_CHARS,
    context: int = CONTEXT_LINES,
) -> tuple[list[Chunk], list[tuple[str, str]]]:
    """Render reviewable files as gutter-numbered chunks with surrounding source; return (chunks, skipped)."""
    chunks: list[Chunk] = []
    skipped: list[tuple[str, str]] = []
    changed = [item.path for item in files]
    for item in files:
        reason = skip_reason(item)
        if reason:
            skipped.append((item.path, reason))
            continue
        source = read_file(item.path)
        others = [path for path in changed if path != item.path]
        listing = ", ".join(others[:30]) + (f" (+{len(others) - 30} more)" if len(others) > 30 else "")
        blocks = _file_hunk_blocks(item, source, max_chars, context)
        groups = _pack_hunk_groups(blocks, max_chars)
        chunks.extend(_render_file_chunks(item, source, groups, listing, len(chunks) + 1))
    return chunks, skipped


def applies(reviewer: Reviewer, chunk: Chunk) -> bool:
    if reviewer.only_paths is not None:
        return chunk.path in reviewer.only_paths
    if chunk.kind == "doc":
        return reviewer.reads_docs or (reviewer.key in ("bugs", "arch") and bool(_OPS_DOC.search(chunk.path)))
    if reviewer.llm_only:
        return bool(_LLM_HINT.search(chunk.text))
    return True


def plan_find(chunks: list[Chunk], reviewers: list[Reviewer], budget_tokens: int) -> dict[str, Any]:
    """Pick (reviewer, chunk) frames that fit the find budget: code first, then tests, then docs."""
    order = {"code": 0, "test": 1, "doc": 2}
    frames: list[tuple[Reviewer, Chunk]] = []
    dropped: list[tuple[Reviewer, Chunk]] = []
    not_applicable: dict[str, int] = {}
    spent = 0
    for chunk in sorted(chunks, key=lambda item: (order[item.kind], item.id)):
        for reviewer in reviewers:
            if not applies(reviewer, chunk):
                if reviewer.only_paths is None:
                    not_applicable[reviewer.key] = not_applicable.get(reviewer.key, 0) + 1
                continue
            cost = _estimate_tokens(finder_task(reviewer), chunk.text, output=1_500)
            if spent + cost > budget_tokens:
                dropped.append((reviewer, chunk))
                continue
            spent += cost
            frames.append((reviewer, chunk))
    # Group by reviewer so frames sharing a task run together (their prompts share a cacheable prefix).
    keys = [*REVIEWERS, LESSONS_KEY, ACCEPTANCE_KEY, REUSE_KEY, CONTRACT_KEY, CONVENTIONS_KEY]
    frames.sort(key=lambda pair: (keys.index(pair[0].key) if pair[0].key in keys else len(keys), pair[1].id))
    return {"frames": frames, "dropped": dropped, "not_applicable": not_applicable, "estimate": spent}


def _read_untracked_file(cwd: str, name: str) -> tuple[FileDiff | None, str | None]:
    try:
        data = Path(cwd, name).read_bytes()
    except OSError:
        return None, None
    if len(data) > MAX_UNTRACKED_BYTES or b"\0" in data[:8192]:
        return None, f"untracked {name} (binary or over {MAX_UNTRACKED_BYTES // 1000} KB)"
    return added_file(name, data.decode("utf-8", "replace")), None


def _untracked(git: Git, cwd: str, paths: list[str]) -> tuple[list[FileDiff], list[str]]:
    code, stdout, _ = git.call("ls-files", "--others", "--exclude-standard", "-z", "--", *paths)
    if code != 0:
        return [], []
    names = [name for name in stdout.split("\0") if name]
    files, notes = [], []
    for name in names[:MAX_UNTRACKED_FILES]:
        file_diff, note = _read_untracked_file(cwd, name)
        if file_diff is not None:
            files.append(file_diff)
        if note:
            notes.append(note)
    if len(names) > MAX_UNTRACKED_FILES:
        notes.append(
            f"{len(names) - MAX_UNTRACKED_FILES} more untracked files (only the first {MAX_UNTRACKED_FILES} were read)"
        )
    return files, notes


def _target_exists(cwd: str, token: str) -> bool:
    return Path(cwd, token).exists()


def _target_is_ref(git: Git, token: str) -> bool:
    return not token.startswith("-") and git.ok("rev-parse", "--verify", "--quiet", f"{token}^{{commit}}")


def _normalize_scope_paths(target_paths: list[str], cwd: str, top: str) -> list[str]:
    prefix = os.path.relpath(cwd, top) if os.path.abspath(cwd) != os.path.abspath(top) else ""
    paths = [os.path.normpath(os.path.join(prefix, path)) if prefix else os.path.normpath(path) for path in target_paths]
    paths = ["" if path == "." else path for path in paths]
    if any(path == "" for path in paths):
        return []
    return [path for path in paths if path]


def _fetch_pr_meta(runner: Runner | None, top: str, pr: int) -> dict[str, Any]:
    code, stdout, _ = (runner or _run_process)(
        ["gh", "pr", "view", str(pr), "--json", "number,title,url,headRefOid,baseRefName"],
        top,
        GH_TIMEOUT_S,
    )
    if code != 0:
        return {}
    try:
        loaded = json.loads(stdout)
    except ValueError:
        loaded = None
    return loaded if isinstance(loaded, dict) else {}


def _pr_reader_and_notes(
    git: Git,
    top: str,
    head: str | None,
) -> tuple[Callable[[str], list[str] | None], str | None, list[str]]:
    if head and git.value("rev-parse", "HEAD") == head:
        return _local_reader(top), None, []
    if head and git.ok("cat-file", "-e", f"{head}^{{commit}}"):
        return _rev_reader(git, head), head, []
    note = (
        "code around the diff and callers (the PR head commit is not in this clone; fetch it or "
        "check out the PR branch to include them)"
    )
    return (lambda _path: None), "", [note]


def _resolve_pr_scope(
    git: Git,
    top: str,
    pr: int,
    paths: list[str],
    where: str,
    *,
    runner: Runner | None,
    which: Callable[[str], str | None],
) -> Scope:
    if which("gh") is None:
        raise ReviewError("reviewing a pull request needs the GitHub CLI (gh), which is not installed")
    code, diff, stderr = (runner or _run_process)(["gh", "pr", "diff", str(pr), "--color=never"], top, GH_TIMEOUT_S)
    if code != 0:
        raise ReviewError(f"gh pr diff {pr} failed: {(stderr or diff).strip()[:300]}")
    meta = _fetch_pr_meta(runner, top, pr)
    files = [item for item in parse_diff(diff) if _within(item.path, paths)]
    reader, grep_rev, notes = _pr_reader_and_notes(git, top, meta.get("headRefOid"))
    title = f": {meta['title']}" if meta.get("title") else ""
    label = f"PR #{pr}{title}{where}"
    return Scope(label, files, reader, top, grep_rev, pr, meta.get("url"), notes)


def _explicit_base_label(git: Git, base_ref: str, has_head: bool, where: str) -> tuple[str, str]:
    base = git.value("merge-base", base_ref, "HEAD") if has_head else None
    base = base or git.out("rev-parse", "--verify", f"{base_ref}^{{commit}}").strip()
    return base, f"changes since {base_ref} (merge base {base[:8]}), including uncommitted{where}"


def _resolve_local_base(git: Git, target: Target, where: str) -> tuple[str, str]:
    has_head = git.ok("rev-parse", "--verify", "--quiet", "HEAD")
    if target.base is not None:
        return _explicit_base_label(git, target.base, has_head, where)
    if not has_head:
        return EMPTY_TREE, f"all files (the repository has no commits yet){where}"
    branch = git.value("branch", "--show-current") or "HEAD"
    default = default_branch(git)
    merge_base = git.value("merge-base", default, "HEAD") if default else None
    head = git.value("rev-parse", "HEAD")
    if merge_base and merge_base != head:
        return merge_base, f"branch {branch} vs {default} (merge base {merge_base[:8]}) plus uncommitted changes{where}"
    return "HEAD", f"uncommitted changes on {branch}{where}"


def resolve_scope(
    options: Options,
    cwd: str,
    *,
    runner: Runner | None = None,
    which: Callable[[str], str | None] = shutil.which,
) -> Scope:
    git = Git(cwd, runner)
    if not git.ok("rev-parse", "--git-dir"):
        raise ReviewError(f"{cwd} is not inside a git repository, so there is no diff to review")
    top = git.value("rev-parse", "--show-toplevel") or cwd
    git = Git(top, runner)
    target = classify_targets(options, exists=partial(_target_exists, cwd), is_ref=partial(_target_is_ref, git))
    # Paths are given relative to cwd; git diff paths are relative to the top level.
    paths = _normalize_scope_paths(target.paths, cwd, top)
    where = f" in {', '.join(target.paths)}" if target.paths else ""
    if target.pr is not None:
        return _resolve_pr_scope(git, top, target.pr, paths, where, runner=runner, which=which)
    base, label = _resolve_local_base(git, target, where)
    diff = git.out(
        "-c", "diff.noprefix=false", "diff", "--no-color", "--no-ext-diff", "--no-relative",
        "--src-prefix=a/", "--dst-prefix=b/", "-M", "-U3", base, "--", *paths,
    )
    files = parse_diff(diff)
    untracked, notes = _untracked(git, top, paths)
    files += untracked
    return Scope(
        label,
        files,
        _local_reader(top),
        top,
        None,
        notes=[f"skipped {note}" for note in notes],
        whole=not paths,
        base=base,
    )


__all__ = [name for name in globals() if not name.startswith("__")]
