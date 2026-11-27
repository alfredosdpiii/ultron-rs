#!/usr/bin/env python3
"""Strip non-exempt inline comments from Rust (.rs) and Python (.py) source files.

Lexer-based: never touches string literals, raw strings (r#*"..."#*), char literals,
nested block comments, shebangs, SPDX/license headers, SAFETY comments, or `ponytail:`
deliberate-simplification ceiling notes.

By default preserves Rust doc comments (`///`, `//!`) and Python docstrings; pass
`--strip-docs` to remove Rust doc comments too, and `--strip-ponytail` to remove
`ponytail:` ceiling notes as well.

Usage:
  scripts/strip-comments.py --check [paths...]         # exit 1 if any non-exempt comments exist
  scripts/strip-comments.py --diff [paths...]          # print unified diff without writing
  scripts/strip-comments.py --apply [paths...]         # rewrite files in place
  scripts/strip-comments.py --changed HEAD --check     # check only git-modified files
  scripts/strip-comments.py --self-test                # run lexer unit tests
"""

from __future__ import annotations

import argparse
import difflib
import io
import os
import re
import subprocess
import sys
import tokenize
from pathlib import Path

KEEP_LINE_PATTERNS = (
    re.compile(r"^\s*(?://|#|/\*)\s*(?:SPDX-License-Identifier:|Copyright\b|License\b)", re.I),
    re.compile(r"^\s*//\s*SAFETY:"),
    re.compile(
        r"^\s*(?://|#)\s*(?:rustfmt::|clippy::|noqa\b|"
        r"type:\s*ignore|fmt:\s*(?:off|on)|pragma:\s*no\s*cover)",
        re.I,
    ),
)
PONYTAIL_PATTERN = re.compile(r"^\s*(?://|#)\s*ponytail:", re.I)
SKIP_DIRS = frozenset(("target", "node_modules", ".git", "__pycache__"))
DEFAULT_SCAN_PATHS = ("crates", "packages/coding-agent/src/ultron/rlm")


def keep_comment(text: str, *, keep_ponytail: bool) -> bool:
    if keep_ponytail and PONYTAIL_PATTERN.match(text):
        return True
    return any(pattern.match(text) for pattern in KEEP_LINE_PATTERNS)


def _is_ident_char(ch: str) -> bool:
    return ch.isalnum() or ch == "_"


def _starts_at_boundary(source: str, index: int) -> bool:
    return index == 0 or not _is_ident_char(source[index - 1])


def _raw_string_prefix_end(source: str, index: int) -> int | None:
    if not _starts_at_boundary(source, index):
        return None
    ch = source[index]
    if ch == "r":
        return index
    if ch in ("b", "c") and source[index + 1 : index + 2] == "r":
        return index + 1
    return None


def _match_raw_string(source: str, index: int) -> int | None:
    r_pos = _raw_string_prefix_end(source, index)
    if r_pos is None:
        return None
    cursor = r_pos + 1
    while cursor < len(source) and source[cursor] == "#":
        cursor += 1
    if source[cursor : cursor + 1] != '"':
        return None
    end_delim = '"' + ("#" * (cursor - r_pos - 1))
    end_pos = source.find(end_delim, cursor + 1)
    if end_pos == -1:
        return len(source)
    return end_pos + len(end_delim)


def _string_content_start(source: str, index: int) -> int | None:
    ch = source[index]
    if ch == '"':
        return index + 1
    if ch in ("b", "c") and source[index + 1 : index + 2] == '"' and _starts_at_boundary(source, index):
        return index + 2
    return None


def _match_quoted_string(source: str, index: int) -> int | None:
    cursor = _string_content_start(source, index)
    if cursor is None:
        return None
    length = len(source)
    while cursor < length:
        ch = source[cursor]
        if ch == "\\":
            cursor += 2
            continue
        cursor += 1
        if ch == '"':
            break
    return cursor


def _char_quote_pos(source: str, index: int) -> int | None:
    ch = source[index]
    if ch == "'":
        return index
    if ch == "b" and source[index + 1 : index + 2] == "'" and _starts_at_boundary(source, index):
        return index + 1
    return None


def _match_escaped_char(source: str, quote_pos: int) -> int | None:
    cursor = quote_pos + 2
    length = len(source)
    while cursor < length and source[cursor] not in ("'", "\n"):
        cursor += 2 if source[cursor] == "\\" else 1
    if source[cursor : cursor + 1] == "'":
        return cursor + 1
    return None


def _match_char_literal(source: str, index: int) -> int | None:
    quote_pos = _char_quote_pos(source, index)
    if quote_pos is None:
        return None
    next_ch = source[quote_pos + 1 : quote_pos + 2]
    if not next_ch:
        return None
    if next_ch == "\\":
        return _match_escaped_char(source, quote_pos)
    if next_ch != "\n" and source[quote_pos + 2 : quote_pos + 3] == "'":
        return quote_pos + 3
    return None


def _match_rust_literal(source: str, index: int) -> int | None:
    for matcher in (_match_raw_string, _match_quoted_string, _match_char_literal):
        end = matcher(source, index)
        if end is not None:
            return end
    return None


def _current_line_prefix(out: list[str]) -> str:
    trailing = "".join(out)
    last_nl = trailing.rfind("\n")
    return trailing[last_nl + 1 :]


def _trim_output_suffix(out: list[str], count: int) -> None:
    remaining = count
    while remaining > 0 and out:
        tail = out[-1]
        if len(tail) <= remaining:
            remaining -= len(tail)
            out.pop()
        else:
            out[-1] = tail[:-remaining]
            remaining = 0


def _trim_trailing_horizontal_ws(out: list[str]) -> None:
    while out and out[-1] and out[-1][-1] in (" ", "\t"):
        trimmed = out[-1].rstrip(" \t")
        if trimmed:
            out[-1] = trimmed
            break
        out.pop()


def _is_rust_line_doc(comment: str) -> bool:
    if comment.startswith("//!"):
        return True
    return comment.startswith("///") and not comment.startswith("////")


def _consume_line_comment(
    source: str,
    index: int,
    out: list[str],
    *,
    strip_docs: bool,
    keep_ponytail: bool,
) -> tuple[int, int]:
    newline_pos = source.find("\n", index)
    end = len(source) if newline_pos == -1 else newline_pos
    comment = source[index:end]
    preserved_doc = _is_rust_line_doc(comment) and not strip_docs
    if preserved_doc or keep_comment(comment, keep_ponytail=keep_ponytail):
        out.append(comment)
        return end, 0

    line_prefix = _current_line_prefix(out)
    if line_prefix.strip() == "":
        _trim_output_suffix(out, len(line_prefix))
        next_index = end + 1 if end < len(source) else len(source)
        return next_index, 1

    _trim_trailing_horizontal_ws(out)
    return end, 1


def _is_rust_block_doc(source: str, index: int) -> bool:
    third = source[index + 2 : index + 3]
    if third == "!":
        return True
    if third != "*":
        return False
    return source[index + 3 : index + 4] not in ("*", "/")


def _scan_block_comment_end(source: str, start: int) -> int:
    depth = 1
    cursor = start + 2
    length = len(source)
    while cursor < length and depth > 0:
        pair = source[cursor : cursor + 2]
        if pair == "/*":
            depth += 1
            cursor += 2
        elif pair == "*/":
            depth -= 1
            cursor += 2
        else:
            cursor += 1
    return cursor


def _consume_block_comment(
    source: str,
    index: int,
    out: list[str],
    *,
    strip_docs: bool,
    keep_ponytail: bool,
) -> tuple[int, int]:
    is_doc = _is_rust_block_doc(source, index)
    end = _scan_block_comment_end(source, index)
    comment = source[index:end]
    if (is_doc and not strip_docs) or keep_comment(comment, keep_ponytail=keep_ponytail):
        out.append(comment)
        return end, 0

    if "\n" in comment and source[end : end + 1] == "\n":
        line_prefix = _current_line_prefix(out)
        if line_prefix.strip() == "":
            _trim_output_suffix(out, len(line_prefix))
            return end + 1, 1
    return end, 1


def strip_rust(source: str, *, strip_docs: bool = False, keep_ponytail: bool = True) -> tuple[str, int]:
    """Tokenize Rust source and remove non-exempt comments without corrupting literals."""
    length = len(source)
    index = 0
    out: list[str] = []
    removed = 0

    while index < length:
        literal_end = _match_rust_literal(source, index)
        if literal_end is not None:
            out.append(source[index:literal_end])
            index = literal_end
            continue

        pair = source[index : index + 2]
        if pair == "//":
            index, delta = _consume_line_comment(
                source, index, out, strip_docs=strip_docs, keep_ponytail=keep_ponytail
            )
            removed += delta
            continue
        if pair == "/*":
            index, delta = _consume_block_comment(
                source, index, out, strip_docs=strip_docs, keep_ponytail=keep_ponytail
            )
            removed += delta
            continue

        out.append(source[index])
        index += 1

    result = re.sub(r"\n{3,}", "\n\n", "".join(out))
    return result, removed


def _is_preserved_python_comment(line_idx: int, text: str, *, keep_ponytail: bool) -> bool:
    if line_idx == 0 and text.startswith("#!"):
        return True
    if line_idx in (0, 1) and re.search(r"coding[:=]\s*[-\w.]+", text):
        return True
    return keep_comment(text, keep_ponytail=keep_ponytail)


def _strip_python_line(line: str, start_col: int) -> str:
    prefix = line[:start_col]
    if prefix.strip() == "":
        return ""
    if line.endswith("\r\n"):
        newline = "\r\n"
    elif line.endswith("\n"):
        newline = "\n"
    else:
        newline = ""
    return prefix.rstrip(" \t") + newline


def strip_python(source: str, *, keep_ponytail: bool = True) -> tuple[str, int]:
    """Tokenize Python source and remove non-exempt `#` comments without touching strings or docstrings."""
    try:
        tokens = list(tokenize.generate_tokens(io.StringIO(source).readline))
    except tokenize.TokenError:
        return source, 0

    to_remove = [
        (tok.start[0] - 1, tok.start[1])
        for tok in tokens
        if tok.type == tokenize.COMMENT
        and not _is_preserved_python_comment(tok.start[0] - 1, tok.string, keep_ponytail=keep_ponytail)
    ]
    if not to_remove:
        return source, 0

    new_lines = source.splitlines(keepends=True)
    for line_idx, start_col in reversed(to_remove):
        new_lines[line_idx] = _strip_python_line(new_lines[line_idx], start_col)

    result = re.sub(r"\n{3,}", "\n\n", "".join(new_lines))
    return result, len(to_remove)


def _changed_files(changed_ref: str) -> list[Path]:
    out = subprocess.check_output(
        ["git", "diff", "--name-only", "--diff-filter=ACMR", changed_ref],
        text=True,
    )
    return [Path(line) for line in out.splitlines() if line.strip()]


def _walk_source_path(path: Path) -> list[Path]:
    if path.is_file():
        return [path]
    if not path.is_dir():
        return []
    found: list[Path] = []
    for root, dirs, files in os.walk(path):
        dirs[:] = sorted(name for name in dirs if name not in SKIP_DIRS)
        found.extend(Path(root) / name for name in sorted(files))
    return found


def iter_files(paths: list[str], changed_ref: str | None) -> list[Path]:
    if changed_ref is not None:
        candidates = _changed_files(changed_ref)
    else:
        candidates = []
        for raw in paths or DEFAULT_SCAN_PATHS:
            candidates.extend(_walk_source_path(Path(raw)))
    return [path for path in candidates if path.is_file() and path.suffix in (".rs", ".py")]


def run_self_test() -> None:
    rs_sample = '''//! Module doc stays.
/// Public doc stays.
// SPDX-License-Identifier: MIT
// SAFETY: pointer is valid
// ponytail: O(n^2) scan; index if > 1000 items
// This inline comment should die.
fn demo<'a>(s: &'a str) -> &'static str {
    let url = "https://example.com//not-a-comment";
    let raw = r#"// also not a comment /* nested */"#;
    let ch = '/'; // trailing comment dies
    /* outer /* nested */ block comment dies */
    let _ = (s, url, raw, ch);
    "ok"
}
'''
    stripped, count = strip_rust(rs_sample)
    assert count == 3, f"expected 3 removed, got {count}\n{stripped}"
    assert "//! Module doc stays." in stripped
    assert "/// Public doc stays." in stripped
    assert "SPDX-License-Identifier: MIT" in stripped
    assert "// SAFETY: pointer is valid" in stripped
    assert "// ponytail: O(n^2) scan" in stripped
    assert '"https://example.com//not-a-comment"' in stripped
    assert 'r#"// also not a comment /* nested */"#' in stripped
    assert "let ch = '/';" in stripped
    assert "This inline comment should die." not in stripped
    assert "trailing comment dies" not in stripped
    assert "block comment dies" not in stripped

    py_sample = '''#!/usr/bin/env python3
"""Docstring stays."""
# ponytail: linear scan
# Inline comment dies.
URL = "https://example.com/#not-a-comment"  # trailing dies
'''
    py_out, py_count = strip_python(py_sample)
    assert py_count == 2, f"expected 2 removed, got {py_count}\n{py_out}"
    assert "#!/usr/bin/env python3" in py_out
    assert '"""Docstring stays."""' in py_out
    assert "# ponytail: linear scan" in py_out
    assert '"https://example.com/#not-a-comment"' in py_out
    assert "Inline comment dies." not in py_out
    assert "trailing dies" not in py_out
    print("strip-comments.py self-test OK")


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Strip non-exempt comments from Rust and Python files.")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true", help="Exit 1 if any non-exempt comments are found")
    mode.add_argument("--diff", action="store_true", help="Show unified diff without modifying files")
    mode.add_argument("--apply", action="store_true", help="Strip non-exempt comments in place")
    mode.add_argument("--self-test", action="store_true", help="Run built-in lexer tests")
    parser.add_argument("--changed", metavar="REF", help="Only process files changed since git REF")
    parser.add_argument("--strip-docs", action="store_true", help="Also strip Rust doc comments (///, //!)")
    parser.add_argument("--strip-ponytail", action="store_true", help="Also strip `ponytail:` comments")
    parser.add_argument("paths", nargs="*", help="Files or directories to scan")
    return parser


def _strip_source(path: Path, text: str, args: argparse.Namespace) -> tuple[str, int]:
    keep_ponytail = not args.strip_ponytail
    if path.suffix == ".rs":
        return strip_rust(text, strip_docs=args.strip_docs, keep_ponytail=keep_ponytail)
    return strip_python(text, keep_ponytail=keep_ponytail)


def _report_file_change(
    path: Path,
    original: str,
    updated: str,
    count: int,
    args: argparse.Namespace,
) -> None:
    if args.apply:
        path.write_text(updated, encoding="utf-8")
    elif args.diff:
        sys.stdout.writelines(
            difflib.unified_diff(
                original.splitlines(keepends=True),
                updated.splitlines(keepends=True),
                fromfile=str(path),
                tofile=str(path),
            )
        )
    elif args.check:
        print(f"{path}: {count} non-exempt comment(s)")


def main() -> int:
    args = _build_parser().parse_args()
    if args.self_test:
        run_self_test()
        return 0

    total_removed = 0
    touched_files = 0
    for path in iter_files(args.paths, args.changed):
        text = path.read_text(encoding="utf-8")
        updated, count = _strip_source(path, text, args)
        if count == 0 or updated == text:
            continue
        total_removed += count
        touched_files += 1
        _report_file_change(path, text, updated, count, args)

    if args.check and total_removed:
        summary = f"Found {total_removed} non-exempt comment(s) across {touched_files} file(s)."
        print(summary, file=sys.stderr)
        return 1
    if args.apply:
        print(f"Stripped {total_removed} comment(s) across {touched_files} file(s).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
