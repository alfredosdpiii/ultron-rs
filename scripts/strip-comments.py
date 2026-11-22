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
    re.compile(r"^\s*(?://|#)\s*(?:rustfmt::|clippy::|noqa\b|type:\s*ignore|fmt:\s*(?:off|on)|pragma:\s*no\s*cover)", re.I),
)
PONYTAIL_PATTERN = re.compile(r"^\s*(?://|#)\s*ponytail:", re.I)


def keep_comment(text: str, *, keep_ponytail: bool) -> bool:
    if keep_ponytail and PONYTAIL_PATTERN.match(text):
        return True
    return any(p.match(text) for p in KEEP_LINE_PATTERNS)


def strip_rust(source: str, *, strip_docs: bool = False, keep_ponytail: bool = True) -> tuple[str, int]:
    """Tokenize Rust source and remove non-exempt comments without corrupting literals."""
    n = len(source)
    i = 0
    out: list[str] = []
    removed = 0
    # Track preceding non-whitespace char to distinguish lifetime `'a` from char literal `'a'`.
    prev_non_ws = ""

    def emit(chunk: str) -> None:
        nonlocal prev_non_ws
        out.append(chunk)
        stripped = chunk.rstrip()
        if stripped:
            prev_non_ws = stripped[-1]

    while i < n:
        ch = source[i]

        # Raw strings: r"...", r#"..."#, br"...", br#"..."#, cr"...", cr#"..."#
        if ch in ("r", "b", "c"):
            j = i
            if ch in ("b", "c") and j + 1 < n and source[j + 1] == "r":
                j += 1
            if source[j] == "r":
                # Ensure this isn't the tail of an identifier.
                if i == 0 or not (source[i - 1].isalnum() or source[i - 1] == "_"):
                    k = j + 1
                    hashes = 0
                    while k < n and source[k] == "#":
                        hashes += 1
                        k += 1
                    if k < n and source[k] == '"':
                        end_delim = '"' + ("#" * hashes)
                        end_pos = source.find(end_delim, k + 1)
                        if end_pos == -1:
                            emit(source[i:])
                            break
                        end_pos += len(end_delim)
                        emit(source[i:end_pos])
                        i = end_pos
                        continue

        # Byte string / C-string / normal string: b"...", c"...", "..."
        if ch == '"' or (ch in ("b", "c") and i + 1 < n and source[i + 1] == '"' and (i == 0 or not (source[i - 1].isalnum() or source[i - 1] == "_"))):
            start = i
            i += 2 if ch in ("b", "c") else 1
            while i < n:
                if source[i] == "\\":
                    i += 2
                    continue
                if source[i] == '"':
                    i += 1
                    break
                i += 1
            emit(source[start:i])
            continue

        # Char or byte char literal: 'a', '\n', '\x7f', '\u{1234}', b'a'
        if ch == "'" or (ch == "b" and i + 1 < n and source[i + 1] == "'" and (i == 0 or not (source[i - 1].isalnum() or source[i - 1] == "_"))):
            start = i
            q = i + 1 if ch == "b" else i
            # Distinguish char literal from lifetime `'ident`: char literal closes with `'` after 1 char or escape.
            if q + 1 < n:
                if source[q + 1] == "\\":
                    k = q + 2
                    while k < n and source[k] != "'" and source[k] != "\n":
                        if source[k] == "\\":
                            k += 2
                            continue
                        k += 1
                    if k < n and source[k] == "'":
                        emit(source[start : k + 1])
                        i = k + 1
                        continue
                elif q + 2 < n and source[q + 2] == "'" and source[q + 1] != "\n":
                    emit(source[start : q + 3])
                    i = q + 3
                    continue
            emit(source[i])
            i += 1
            continue

        # Line comments: //, ///, //!
        if ch == "/" and i + 1 < n and source[i + 1] == "/":
            end = source.find("\n", i)
            if end == -1:
                end = n
            comment = source[i:end]
            is_doc = (
                comment.startswith("///") and not comment.startswith("////")
            ) or comment.startswith("//!")
            if (is_doc and not strip_docs) or keep_comment(comment, keep_ponytail=keep_ponytail):
                emit(comment)
                i = end
                continue
            # Remove this comment. If it occupied the whole line (only indentation before it), remove the whole line.
            removed += 1
            trailing = "".join(out)
            last_nl = trailing.rfind("\n")
            line_prefix = trailing[last_nl + 1 :]
            if line_prefix.strip() == "":
                # Drop indentation emitted for this comment line and its newline.
                to_trim = len(line_prefix)
                while to_trim > 0 and out:
                    if len(out[-1]) <= to_trim:
                        to_trim -= len(out[-1])
                        out.pop()
                    else:
                        out[-1] = out[-1][:-to_trim]
                        to_trim = 0
                i = end + 1 if end < n else n
            else:
                # Trailing comment after code: trim trailing horizontal spaces before it.
                while out and out[-1] and out[-1][-1] in (" ", "\t"):
                    out[-1] = out[-1].rstrip(" \t")
                    if out[-1] == "":
                        out.pop()
                i = end
            continue

        # Block comments (Rust supports nesting): /* ... */
        if ch == "/" and i + 1 < n and source[i + 1] == "*":
            start = i
            is_doc = (
                i + 2 < n
                and source[i + 2] in ("*", "!")
                and not (source[i + 2] == "*" and i + 3 < n and source[i + 3] in ("*", "/"))
            )
            depth = 1
            i += 2
            while i < n and depth > 0:
                if source[i] == "/" and i + 1 < n and source[i + 1] == "*":
                    depth += 1
                    i += 2
                elif source[i] == "*" and i + 1 < n and source[i + 1] == "/":
                    depth -= 1
                    i += 2
                else:
                    i += 1
            comment = source[start:i]
            if (is_doc and not strip_docs) or keep_comment(comment, keep_ponytail=keep_ponytail):
                emit(comment)
                continue
            removed += 1
            if "\n" in comment:
                # Preserve line separation if code precedes and follows.
                trailing = "".join(out)
                last_nl = trailing.rfind("\n")
                line_prefix = trailing[last_nl + 1 :]
                if line_prefix.strip() == "" and i < n and source[i] == "\n":
                    to_trim = len(line_prefix)
                    while to_trim > 0 and out:
                        if len(out[-1]) <= to_trim:
                            to_trim -= len(out[-1])
                            out.pop()
                        else:
                            out[-1] = out[-1][:-to_trim]
                            to_trim = 0
                    i += 1
            continue

        emit(ch)
        i += 1

    result = "".join(out)
    # Collapse runs of 3+ blank lines created by removing comment blocks.
    result = re.sub(r"\n{3,}", "\n\n", result)
    return result, removed


def strip_python(source: str, *, keep_ponytail: bool = True) -> tuple[str, int]:
    """Tokenize Python source and remove non-exempt `#` comments without touching strings or docstrings."""
    lines = source.splitlines(keepends=True)
    to_remove: list[tuple[int, int, int]] = []  # (line_idx_0, start_col, end_col)
    try:
        tokens = list(tokenize.generate_tokens(io.StringIO(source).readline))
    except tokenize.TokenError:
        return source, 0

    for tok in tokens:
        if tok.type != tokenize.COMMENT:
            continue
        line_idx = tok.start[0] - 1
        text = tok.string
        # Always preserve shebang on line 1 and encoding cookies on lines 1-2.
        if line_idx == 0 and text.startswith("#!"):
            continue
        if line_idx in (0, 1) and re.search(r"coding[:=]\s*[-\w.]+", text):
            continue
        if keep_comment(text, keep_ponytail=keep_ponytail):
            continue
        to_remove.append((line_idx, tok.start[1], tok.end[1]))

    if not to_remove:
        return source, 0

    removed = len(to_remove)
    new_lines = list(lines)
    for line_idx, start_col, _end_col in reversed(to_remove):
        line = new_lines[line_idx]
        nl = "\r\n" if line.endswith("\r\n") else ("\n" if line.endswith("\n") else "")
        prefix = line[:start_col]
        if prefix.strip() == "":
            new_lines[line_idx] = ""
        else:
            new_lines[line_idx] = prefix.rstrip(" \t") + nl

    result = "".join(new_lines)
    result = re.sub(r"\n{3,}", "\n\n", result)
    return result, removed


def iter_files(paths: list[str], changed_ref: str | None) -> list[Path]:
    if changed_ref is not None:
        out = subprocess.check_output(
            ["git", "diff", "--name-only", "--diff-filter=ACMR", changed_ref],
            text=True,
        )
        candidates = [Path(p) for p in out.splitlines() if p.strip()]
    else:
        candidates = []
        for raw in paths or ["crates", "packages/coding-agent/src/ultron/rlm"]:
            p = Path(raw)
            if p.is_file():
                candidates.append(p)
            elif p.is_dir():
                for root, dirs, files in os.walk(p):
                    dirs[:] = sorted(d for d in dirs if d not in ("target", "node_modules", ".git", "__pycache__"))
                    for name in sorted(files):
                        candidates.append(Path(root) / name)
    return [p for p in candidates if p.is_file() and p.suffix in (".rs", ".py")]


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


def main() -> int:
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
    args = parser.parse_args()

    if args.self_test:
        run_self_test()
        return 0

    files = iter_files(args.paths, args.changed)
    total_removed = 0
    touched_files = 0
    for path in files:
        text = path.read_text(encoding="utf-8")
        if path.suffix == ".rs":
            updated, count = strip_rust(
                text,
                strip_docs=args.strip_docs,
                keep_ponytail=not args.strip_ponytail,
            )
        else:
            updated, count = strip_python(text, keep_ponytail=not args.strip_ponytail)
        if count == 0 or updated == text:
            continue
        total_removed += count
        touched_files += 1
        if args.apply:
            path.write_text(updated, encoding="utf-8")
        elif args.diff:
            sys.stdout.writelines(
                difflib.unified_diff(
                    text.splitlines(keepends=True),
                    updated.splitlines(keepends=True),
                    fromfile=str(path),
                    tofile=str(path),
                )
            )
        elif args.check:
            print(f"{path}: {count} non-exempt comment(s)")

    if args.check:
        if total_removed:
            print(f"Found {total_removed} non-exempt comment(s) across {touched_files} file(s).", file=sys.stderr)
            return 1
        return 0
    if args.apply:
        print(f"Stripped {total_removed} comment(s) across {touched_files} file(s).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
