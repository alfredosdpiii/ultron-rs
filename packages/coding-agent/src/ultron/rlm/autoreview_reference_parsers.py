"""Language parsers and indexers for `autoreview_reference` (Python, TS/JS, YAML, Terraform)."""
from __future__ import annotations

import ast
import json
import re
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any

from autoreview_reference_types import (
    MAX_BYTES,
    MAX_CALLS_PER_FILE,
    MAX_FAMILIES_PER_FILE,
    MAX_FILES,
    MIN_FAMILY_ITEMS,
    CallSite,
    Family,
    FileIndex,
    Symbol,
    TfBlock,
    YamlNode,
    _clip,
    _TF_BLOCK,
)

# The compiler host sees only commit contents supplied by Reference, never the checkout or tsconfig.
_TS_CHECKER = r"""
const ts = require(process.argv[1]);
const input = JSON.parse(require('node:fs').readFileSync(0, 'utf8'));
function index(files) {
  const texts = new Map(Object.entries(files).map(([p, text]) => ['/' + p, text]));
  const options = {allowJs: true, checkJs: true, noLib: true, noResolve: false,
    target: ts.ScriptTarget.Latest, module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler, jsx: ts.JsxEmit.Preserve};
  const host = {
    getSourceFile: (p, version) => texts.has(p) ? ts.createSourceFile(p, texts.get(p), version, true) : undefined,
    getDefaultLibFileName: () => '', writeFile: () => {}, getCurrentDirectory: () => '/',
    getDirectories: () => [], fileExists: p => texts.has(p), readFile: p => texts.get(p),
    getCanonicalFileName: p => p, useCaseSensitiveFileNames: () => true, getNewLine: () => '\n'
  };
  const program = ts.createProgram([...texts.keys()], options, host);
  const checker = program.getTypeChecker();
  const result = Object.create(null), definitions = new Map(), scopes = new Set();
  const clip = s => s.trim().slice(0, 160);
  for (const sf of program.getSourceFiles()) {
    const path = sf.fileName.slice(1);
    const out = result[path] = {symbols: [], calls: [], imports: {}, exports: [], parsed: !sf.parseDiagnostics.length};
    const line = node => sf.getLineAndCharacterOfPosition(node.getStart(sf)).line + 1;
    function visit(node, scope = '', parent = null) {
      let kind;
      if (ts.isFunctionDeclaration(node)) kind = 'function';
      else if (ts.isMethodDeclaration(node) || ts.isGetAccessor(node) || ts.isSetAccessor(node)) kind = 'method';
      else if (ts.isClassDeclaration(node)) kind = 'class';
      else if (ts.isInterfaceDeclaration(node)) kind = 'interface';
      else if (ts.isTypeAliasDeclaration(node)) kind = 'type';
      else if (ts.isEnumDeclaration(node)) kind = 'enum';
      else if (ts.isVariableDeclaration(node) || ts.isPropertyDeclaration(node)) {
        kind = node.initializer && (ts.isArrowFunction(node.initializer) || ts.isFunctionExpression(node.initializer))
          ? (parent ? 'method' : 'function') : 'variable';
      }
      const name = node.name && (ts.isIdentifier(node.name) || ts.isStringLiteral(node.name))
        ? node.name.text : (kind && node.modifiers?.some(m => m.kind === ts.SyntaxKind.DefaultKeyword) ? 'default' : null);
      if (kind && name) {
        if (out.symbols.length >= 4000) throw Error('symbol bound');
        const qualname = scope ? scope + '.' + name : name;
        const callable = node.initializer && (ts.isArrowFunction(node.initializer) || ts.isFunctionExpression(node.initializer))
          ? node.initializer : node;
        const signature = callable.parameters ? checker.getSignatureFromDeclaration(callable) : undefined;
        const exported = !!node.modifiers?.some(m => m.kind === ts.SyntaxKind.ExportKeyword)
          || (ts.isVariableDeclaration(node) && !!node.parent.parent.modifiers?.some(m => m.kind === ts.SyntaxKind.ExportKeyword));
        const symbol = {path, name, qualname, kind, line: line(node),
          end: sf.getLineAndCharacterOfPosition(node.end).line + 1,
          signature: clip(signature ? name + checker.signatureToString(signature) : node.getText(sf).split(/[\n{]/)[0]),
          parent, exported, bases: (node.heritageClauses || []).flatMap(h => h.types.map(t => t.expression.getText(sf)))};
        out.symbols.push(symbol);
        definitions.set(node, path + '::' + qualname);
        if (['function', 'method', 'class'].includes(kind)) scopes.add(node);
        if (callable !== node) definitions.set(callable, path + '::' + qualname);
        scope = qualname;
        parent = kind === 'class' ? name : null;
      }
      ts.forEachChild(node, child => visit(child, scope, parent));
    }
    visit(sf);
    const module = checker.getSymbolAtLocation(sf);
    if (module) {
      for (const symbol of checker.getExportsOfModule(module)) {
        out.exports.push(symbol.name);
        const d = declaration(symbol);
        if (d && texts.has(d.getSourceFile().fileName)) {
          out.imports[symbol.name] = [d.getSourceFile().fileName.slice(1), d.name?.text || 'default'];
        }
      }
    }
  }
  function declaration(symbol) {
    if (symbol && symbol.flags & ts.SymbolFlags.Alias) symbol = checker.getAliasedSymbol(symbol);
    return symbol && (symbol.valueDeclaration || symbol.declarations?.[0]);
  }
  for (const sf of program.getSourceFiles()) {
    const out = result[sf.fileName.slice(1)], lines = sf.text.split(/\r?\n/);
    function visit(node, enclosing = null) {
      if (scopes.has(node)) enclosing = definitions.get(node).split('::')[1];
      if (ts.isImportDeclaration(node) && node.importClause) {
        const clause = node.importClause;
        const bindings = [clause.name, ...(clause.namedBindings ?
          ts.isNamespaceImport(clause.namedBindings) ? [clause.namedBindings.name] : clause.namedBindings.elements.map(e => e.name) : [])];
        for (const binding of bindings.filter(Boolean)) {
          const d = declaration(checker.getSymbolAtLocation(binding));
          if (d && texts.has(d.getSourceFile().fileName)) {
            out.imports[binding.text] = [d.getSourceFile().fileName.slice(1), d.name?.text || '*'];
          }
        }
      }
      if ((ts.isCallExpression(node) || ts.isNewExpression(node)) && out.calls.length < 2000) {
        const signature = checker.getResolvedSignature(node);
        let d = signature?.declaration;
        if (!d) d = declaration(checker.getSymbolAtLocation(node.expression));
        let target = definitions.get(d);
        if (!target && d && ts.isConstructorDeclaration(d)) target = definitions.get(d.parent);
        const line = sf.getLineAndCharacterOfPosition(node.getStart(sf)).line + 1;
        out.calls.push({path: sf.fileName.slice(1), line, callee: clip(node.expression.getText(sf)),
          text: clip(lines[line - 1]), enclosing, targets: target ? [target] : []});
      }
      ts.forEachChild(node, child => visit(child, enclosing));
    }
    visit(sf);
  }
  return result;
}
const output = JSON.stringify(input.map(index));
if (Buffer.byteLength(output) > 8000000) throw Error('output bound');
process.stdout.write(output);
"""

_TS_CHECKER_OUTPUT_LIMIT = 8_000_000


def _ts_tools(root: str) -> tuple[str, str] | None:
    """Node and a `typescript.js` from PATH, outside the reviewed tree."""
    reviewed = Path(root).resolve()

    def trusted(tool: str) -> Path | None:
        executable = shutil.which(tool)
        if not executable:
            return None
        path = Path(executable)
        resolved = path.resolve()
        if path.absolute().is_relative_to(reviewed) or resolved.is_relative_to(reviewed):
            return None
        return resolved

    node = trusted("node")
    if node is None:
        return None
    tsc = trusted("tsc")
    candidates = ([tsc.parent.parent / "lib/typescript.js"] if tsc else []) + [
        node.parent.parent / "lib/node_modules/typescript/lib/typescript.js"
    ]
    for compiler in (candidate.resolve() for candidate in candidates):
        if compiler.is_file() and not compiler.is_relative_to(reviewed):
            return str(node), str(compiler)
    return None


def _run_ts_checker(tools: tuple[str, str], payload: bytes) -> bytes | None:
    with tempfile.TemporaryDirectory(prefix="ultron-ts-index-") as home:
        run = subprocess.run(
            [tools[0], "--max-old-space-size=256", "-e", _TS_CHECKER, tools[1]],
            input=payload,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=20,
            cwd=home,
            env={"HOME": home},
        )
    if run.returncode or len(run.stdout) > _TS_CHECKER_OUTPUT_LIMIT:
        return None
    return run.stdout


def _decode_ts_group(
    sources_group: dict[str, str],
    group: dict[str, Any],
) -> dict[str, FileIndex] | None:
    if set(group) != set(sources_group):
        return None
    indexes: dict[str, FileIndex] = {}
    for path, data in group.items():
        if not data["parsed"]:
            continue
        indexes[path] = FileIndex(
            path,
            "ts",
            symbols=[Symbol(**sym_data) for sym_data in data["symbols"]],
            imports={local: tuple(target) for local, target in data["imports"].items()},
            calls=[CallSite(**call_data) for call_data in data["calls"]],
            exports=set(data["exports"]),
            checked=True,
        )
    return indexes


def index_ts_checked(
    root: str,
    sources: list[dict[str, str]],
) -> list[dict[str, FileIndex]] | None:
    """One bounded, isolated Node invocation; None leaves every scanner result intact."""
    try:
        tools = _ts_tools(root)
        if tools is None or sum(len(group) for group in sources) > MAX_FILES * 2:
            return None
        payload = json.dumps(sources).encode()
        if len(payload) > MAX_BYTES * 4:
            return None
        stdout = _run_ts_checker(tools, payload)
        if stdout is None:
            return None
        groups = json.loads(stdout)
        if len(groups) != len(sources):
            return None
        result: list[dict[str, FileIndex]] = []
        for sources_group, group in zip(sources, groups):
            decoded = _decode_ts_group(sources_group, group)
            if decoded is None:
                return None
            result.append(decoded)
        return result
    except (OSError, ValueError, TypeError, KeyError, subprocess.TimeoutExpired):
        return None


def _py_signature(node: ast.AST) -> str:
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
        prefix = "async def" if isinstance(node, ast.AsyncFunctionDef) else "def"
        args = ast.unparse(node.args)
        returns = f" -> {ast.unparse(node.returns)}" if node.returns is not None else ""
        return f"{prefix} {node.name}({args}){returns}"
    if isinstance(node, ast.ClassDef):
        bases = ", ".join(ast.unparse(base) for base in node.bases)
        return f"class {node.name}({bases})" if bases else f"class {node.name}"
    return ""


def _py_name(node: ast.AST) -> str | None:
    """The dotted text of a call's callee, or None when it is not a plain name or attribute chain."""
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        base = _py_name(node.value)
        return f"{base}.{node.attr}" if base else None
    return None


def _string_constant_tuple(elements: list[ast.AST | None]) -> tuple[str, ...] | None:
    items: list[str] = []
    for element in elements:
        if not (isinstance(element, ast.Constant) and isinstance(element.value, str)):
            return None
        items.append(element.value)
    return tuple(items) if len(items) >= MIN_FAMILY_ITEMS else None


def _py_family(node: ast.AST) -> tuple[str, ...] | None:
    if isinstance(node, (ast.List, ast.Tuple, ast.Set)):
        return _string_constant_tuple(list(node.elts))
    if isinstance(node, ast.Dict) and node.keys:
        return _string_constant_tuple(list(node.keys))
    return None


class _PyVisitor(ast.NodeVisitor):
    def __init__(self, index: FileIndex, lines: list[str]) -> None:
        self.index = index
        self.lines = lines
        self.stack: list[ast.AST] = []

    def _qual(self, name: str) -> str:
        parts = [
            node.name
            for node in self.stack
            if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef))
        ]
        return ".".join([*parts, name])

    def _enclosing(self) -> str | None:
        parts = [
            node.name
            for node in self.stack
            if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef))
        ]
        return ".".join(parts) or None

    def _parent_class(self) -> str | None:
        for node in reversed(self.stack):
            if isinstance(node, ast.ClassDef):
                return node.name
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                return None
        return None

    def visit_Import(self, node: ast.Import) -> None:
        for alias in node.names:
            local = alias.asname or alias.name.split(".")[0]
            self.index.imports[local] = (None, alias.name)
        self.generic_visit(node)

    def visit_ImportFrom(self, node: ast.ImportFrom) -> None:
        module = "." * (node.level or 0) + (node.module or "")
        for alias in node.names:
            local = alias.asname or alias.name
            self.index.imports[local] = (None, f"{module}:{alias.name}")
        self.generic_visit(node)

    def _add_def(self, node: ast.AST, kind: str) -> None:
        name = getattr(node, "name", "")
        parent = self._parent_class()
        doc = ""
        if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            doc = _clip((ast.get_docstring(node) or "").split("\n")[0], 120)
        end_line = getattr(node, "end_lineno", node.lineno) or node.lineno
        symbol = Symbol(
            self.index.path,
            name,
            self._qual(name),
            "method" if kind == "function" and parent else kind,
            node.lineno,
            end_line,
            _py_signature(node),
            parent,
            [ast.unparse(base) for base in getattr(node, "bases", [])],
            [ast.unparse(item) for item in getattr(node, "decorator_list", [])][:4],
            doc,
            exported=not name.startswith("_"),
        )
        self.index.symbols.append(symbol)
        if not self.stack:
            self.index.exports.add(name)

    def visit_FunctionDef(self, node: ast.FunctionDef) -> None:
        self._add_def(node, "function")
        self.stack.append(node)
        self.generic_visit(node)
        self.stack.pop()

    def visit_AsyncFunctionDef(self, node: ast.AsyncFunctionDef) -> None:
        self._add_def(node, "function")
        self.stack.append(node)
        self.generic_visit(node)
        self.stack.pop()

    def visit_ClassDef(self, node: ast.ClassDef) -> None:
        self._add_def(node, "class")
        self.stack.append(node)
        self.generic_visit(node)
        self.stack.pop()

    def _record_assigned_family(
        self,
        value: ast.AST | None,
        names: list[str],
        lineno: int,
    ) -> None:
        if value is None or len(self.index.families) >= MAX_FAMILIES_PER_FILE:
            return
        family = _py_family(value)
        if family is None:
            return
        self.index.families.append(
            Family(self.index.path, lineno, names[0] if names else None, family, self._enclosing())
        )

    def _record_assigned_symbols(self, names: list[str], node: ast.AST) -> None:
        if self.stack and not isinstance(self.stack[-1], ast.ClassDef):
            return
        end_line = getattr(node, "end_lineno", node.lineno) or node.lineno
        sig = _clip(self.lines[node.lineno - 1], 120) if node.lineno <= len(self.lines) else ""
        parent = self._parent_class()
        for name in names:
            kind = "constant" if name.isupper() else "variable"
            symbol = Symbol(
                self.index.path, name, self._qual(name), kind, node.lineno, end_line,
                sig, parent, exported=not name.startswith("_"),
            )
            self.index.symbols.append(symbol)
            if not self.stack:
                self.index.exports.add(name)

    def _assign(self, targets: list[ast.AST], value: ast.AST | None, node: ast.AST) -> None:
        names = [target.id for target in targets if isinstance(target, ast.Name)]
        self._record_assigned_family(value, names, node.lineno)
        self._record_assigned_symbols(names, node)

    def visit_Assign(self, node: ast.Assign) -> None:
        self._assign(node.targets, node.value, node)
        self.generic_visit(node)

    def visit_AnnAssign(self, node: ast.AnnAssign) -> None:
        self._assign([node.target], node.value, node)
        self.generic_visit(node)

    def visit_Call(self, node: ast.Call) -> None:
        callee = _py_name(node.func)
        if callee and len(self.index.calls) < MAX_CALLS_PER_FILE:
            text = self.lines[node.lineno - 1] if node.lineno <= len(self.lines) else ""
            self.index.calls.append(
                CallSite(self.index.path, node.lineno, callee, _clip(text), self._enclosing())
            )
        self.generic_visit(node)

    def _record_unassigned_family(self, node: ast.AST) -> None:
        if not isinstance(node, (ast.List, ast.Tuple, ast.Set)):
            return
        if isinstance(getattr(node, "ctx", None), ast.Store):
            return
        if len(self.index.families) >= MAX_FAMILIES_PER_FILE:
            return
        family = _py_family(node)
        if family is None:
            return
        duplicate = any(
            item.line == node.lineno and item.items == family for item in self.index.families
        )
        if not duplicate:
            self.index.families.append(
                Family(self.index.path, node.lineno, None, family, self._enclosing())
            )

    def generic_visit(self, node: ast.AST) -> None:
        # Families that are not assigned (a parametrize list, a call argument) still count.
        self._record_unassigned_family(node)
        super().generic_visit(node)


def index_python(path: str, text: str) -> FileIndex:
    index = FileIndex(path, "python")
    try:
        tree = ast.parse(text)
    except (SyntaxError, ValueError, RecursionError):
        index.parsed = False
        return index
    try:
        _PyVisitor(index, text.splitlines()).visit(tree)
    except RecursionError:
        index.parsed = False
    return index


_TS_FUNC = re.compile(
    r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)\s*[<(]"
)
_TS_ARROW = re.compile(
    r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?="
    r"\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*(?::\s*[^=]+)?=>"
)
_TS_CLASS = re.compile(
    r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)"
    r"(?:\s+extends\s+([\w$.]+))?"
)
_TS_TYPE = re.compile(
    r"^\s*(?:export\s+)?(?:declare\s+)?(interface|type|enum)\s+([A-Za-z_$][\w$]*)"
)
_TS_CONST = re.compile(r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=")
_TS_METHOD = re.compile(
    r"^\s+(?:(?:public|private|protected|static|async|readonly|override|get|set)\s+)*"
    r"([A-Za-z_$][\w$]*)\s*(?:<[^>]*>)?\([^)]*\)\s*(?::\s*[^{;=]+)?\{"
)
_TS_IMPORT = re.compile(
    r"""import\s+(?:type\s+)?(?:([A-Za-z_$][\w$]*)\s*,?\s*)?(?:\{([^}]*)\})?\s*"""
    r"""(?:\*\s+as\s+([A-Za-z_$][\w$]*))?\s*from\s*['"]([^'"]+)['"]"""
)
_TS_REQUIRE = re.compile(
    r"""(?:const|let|var)\s+(?:\{([^}]*)\}|([A-Za-z_$][\w$]*))\s*=\s*require\(\s*['"]([^'"]+)['"]\s*\)"""
)
_TS_EXPORT = re.compile(r"^\s*export\s+\{([^}]*)\}")
_TS_ARRAY = re.compile(
    r"\[\s*((?:['\"`][^'\"`\n]*['\"`]\s*,\s*){1,}['\"`][^'\"`\n]*['\"`]\s*,?\s*)\]",
    re.S,
)
_TS_CALL = re.compile(r"(?<![\w$])((?:[A-Za-z_$][\w$]*\.)*[A-Za-z_$][\w$]*)\s*\(")
_TS_KEYWORDS = {
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "function",
    "return",
    "new",
    "typeof",
    "await",
    "else",
    "do",
    "try",
}


def _scan_line_braces(line: str, depth: int, opened: bool) -> tuple[int, bool, bool]:
    for char in line:
        if char == "{":
            depth += 1
            opened = True
        elif char == "}":
            depth -= 1
            if opened and depth <= 0:
                return depth, opened, True
    return depth, opened, False


def _ts_block_end(lines: list[str], start: int) -> int:
    """Line where the block opened on `start` (1-based) closes, by brace count."""
    depth = 0
    opened = False
    for number in range(start, len(lines) + 1):
        line = re.sub(
            r"//.*$|'(?:\\.|[^'\\])*'|\"(?:\\.|[^\"\\])*\"|`(?:\\.|[^`\\])*`",
            "",
            lines[number - 1],
        )
        depth, opened, closed = _scan_line_braces(line, depth, opened)
        if closed:
            return number
        if not opened and number > start:
            return start
    return len(lines)


def _record_ts_symbol(
    index: FileIndex,
    symbol: Symbol,
    exported: bool,
) -> None:
    index.symbols.append(symbol)
    if exported:
        index.exports.add(symbol.name)


def _match_ts_class_or_func(
    index: FileIndex,
    lines: list[str],
    number: int,
    line: str,
    class_stack: list[tuple[str, int]],
    exported: bool,
) -> bool:
    match = _TS_CLASS.match(line)
    if match:
        end = _ts_block_end(lines, number)
        bases = [match.group(2)] if match.group(2) else []
        symbol = Symbol(
            index.path, match.group(1), match.group(1), "class",
            number, end, _clip(line, 120), None, bases, exported=exported,
        )
        _record_ts_symbol(index, symbol, exported)
        class_stack.append((match.group(1), end))
        return True

    match = _TS_FUNC.match(line) or _TS_ARROW.match(line)
    if match:
        name = match.group(1)
        end = _ts_block_end(lines, number)
        parent = class_stack[-1][0] if class_stack else None
        qualname = f"{parent}.{name}" if parent else name
        kind = "method" if parent else "function"
        symbol = Symbol(
            index.path, name, qualname, kind,
            number, end, _clip(line, 120), parent, exported=exported,
        )
        _record_ts_symbol(index, symbol, exported)
        return True
    return False


def _match_ts_method(
    index: FileIndex,
    lines: list[str],
    number: int,
    line: str,
    parent: str,
) -> bool:
    match = _TS_METHOD.match(line)
    if not match or match.group(1) in _TS_KEYWORDS:
        return False
    end = _ts_block_end(lines, number)
    symbol = Symbol(
        index.path, match.group(1), f"{parent}.{match.group(1)}", "method",
        number, end, _clip(line, 120), parent,
    )
    index.symbols.append(symbol)
    return True


def _match_ts_other_symbol(
    index: FileIndex,
    lines: list[str],
    number: int,
    line: str,
    class_stack: list[tuple[str, int]],
    exported: bool,
) -> None:
    match = _TS_TYPE.match(line)
    if match:
        end = _ts_block_end(lines, number)
        symbol = Symbol(
            index.path, match.group(2), match.group(2), match.group(1),
            number, end, _clip(line, 120), exported=exported,
        )
        _record_ts_symbol(index, symbol, exported)
        return

    if class_stack and _match_ts_method(index, lines, number, line, class_stack[-1][0]):
        return

    match = _TS_CONST.match(line)
    if match:
        name = match.group(1)
        kind = "constant" if name.isupper() else "variable"
        symbol = Symbol(
            index.path, name, name, kind, number, number, _clip(line, 120), exported=exported,
        )
        _record_ts_symbol(index, symbol, exported)


def _scan_ts_symbols(index: FileIndex, lines: list[str]) -> None:
    class_stack: list[tuple[str, int]] = []
    for number, line in enumerate(lines, 1):
        while class_stack and number > class_stack[-1][1]:
            class_stack.pop()
        exported = line.lstrip().startswith("export")
        if _match_ts_class_or_func(index, lines, number, line, class_stack, exported):
            continue
        _match_ts_other_symbol(index, lines, number, line, class_stack, exported)


def _add_ts_named_bindings(
    index: FileIndex,
    names: str | None,
    spec: str,
    separator: str,
    strip_type: bool,
) -> None:
    if not names:
        return
    for raw_part in names.split(","):
        part = raw_part.strip()
        if strip_type:
            part = part.removeprefix("type ").strip()
        if not part:
            continue
        original, _, alias = part.partition(separator)
        index.imports[(alias or original).strip()] = (None, f"{spec}:{original.strip()}")


def _scan_ts_imports_and_exports(index: FileIndex, text: str) -> None:
    for match in _TS_IMPORT.finditer(text):
        default, names, star, spec = match.groups()
        if default:
            index.imports[default] = (None, f"{spec}:default")
        if star:
            index.imports[star] = (None, f"{spec}:*")
        _add_ts_named_bindings(index, names, spec, " as ", strip_type=True)

    for match in _TS_REQUIRE.finditer(text):
        names, single, spec = match.groups()
        if single:
            index.imports[single] = (None, f"{spec}:*")
        _add_ts_named_bindings(index, names, spec, ":", strip_type=False)

    for match in _TS_EXPORT.finditer(text):
        for part in match.group(1).split(","):
            name = part.strip().split(" as ")[-1].strip()
            if name:
                index.exports.add(name)


def _scan_ts_calls(index: FileIndex, lines: list[str]) -> None:
    enclosing: list[tuple[Symbol, int]] = []
    symbols_by_line: dict[int, list[Symbol]] = {}
    for symbol in index.symbols:
        if symbol.kind in ("function", "method", "class"):
            symbols_by_line.setdefault(symbol.line, []).append(symbol)

    for number, line in enumerate(lines, 1):
        for symbol in symbols_by_line.get(number, ()):
            enclosing.append((symbol, symbol.end))
        enclosing = [(symbol, end) for symbol, end in enclosing if number <= end]
        inner = enclosing[-1][0] if enclosing else None
        _extract_ts_line_calls(index, number, line, inner)


def _extract_ts_line_calls(
    index: FileIndex,
    number: int,
    line: str,
    inner: Symbol | None,
) -> None:
    if inner is not None and inner.line == number:
        return
    stripped = re.sub(r"//.*$", "", line)
    for match in _TS_CALL.finditer(stripped):
        callee = match.group(1)
        leaf = callee.rsplit(".", 1)[-1]
        if leaf in _TS_KEYWORDS or len(index.calls) >= MAX_CALLS_PER_FILE:
            continue
        index.calls.append(
            CallSite(index.path, number, callee, _clip(line), inner.qualname if inner else None)
        )


def _scan_ts_families(index: FileIndex, text: str) -> None:
    for match in _TS_ARRAY.finditer(text):
        items = tuple(
            item.strip()[1:-1]
            for item in re.findall(r"""['"`][^'"`\n]*['"`]""", match.group(1))
        )
        if len(items) < MIN_FAMILY_ITEMS or len(index.families) >= MAX_FAMILIES_PER_FILE:
            continue
        number = text.count("\n", 0, match.start()) + 1
        before = text[max(0, match.start() - 120):match.start()]
        named = re.search(r"([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*$", before)
        index.families.append(Family(index.path, number, named.group(1) if named else None, items))


def index_ts(path: str, text: str) -> FileIndex:
    index = FileIndex(path, "ts")
    lines = text.splitlines()
    _scan_ts_symbols(index, lines)
    _scan_ts_imports_and_exports(index, text)
    _scan_ts_calls(index, lines)
    _scan_ts_families(index, text)
    return index


def _yaml_quoted(text: str) -> str:
    inner = text[1:-1]
    return inner.replace("''", "'") if text[0] == "'" else re.sub(r"\\(.)", r"\1", inner)


def _yaml_flow_list(inner: str) -> list[Any]:
    trimmed = inner.strip()
    return [] if not trimmed else [_yaml_scalar(part) for part in _split_flow(trimmed)]


def _yaml_flow_dict(inner: str) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for part in _split_flow(inner):
        key, _, value = part.partition(":")
        if key.strip():
            out[_yaml_scalar(key)] = _yaml_scalar(value)
    return out


def _yaml_scalar(text: str) -> Any:
    text = text.strip()
    if text in ("", "~", "null", "Null", "NULL"):
        return None
    if len(text) >= 2 and text[0] == text[-1] and text[0] in "'\"":
        return _yaml_quoted(text)
    if text.startswith("[") and text.endswith("]"):
        return _yaml_flow_list(text[1:-1])
    if text.startswith("{") and text.endswith("}"):
        return _yaml_flow_dict(text[1:-1])
    return text


def _split_flow(text: str) -> list[str]:
    """Top-level comma-separated parts of a flow collection's inside."""
    parts: list[str] = []
    depth = 0
    quote: str | None = None
    current = ""
    for char in text:
        if quote:
            if char == quote:
                quote = None
        elif char in "'\"":
            quote = char
        elif char in "[{":
            depth += 1
        elif char in "]}":
            depth -= 1
        elif char == "," and depth == 0:
            parts.append(current)
            current = ""
            continue
        current += char
    if current.strip():
        parts.append(current)
    return parts


def _strip_comment(line: str) -> str:
    out: list[str] = []
    quote: str | None = None
    for index, char in enumerate(line):
        if quote:
            if char == quote:
                quote = None
        elif char in "'\"":
            quote = char
        elif char == "#" and (index == 0 or line[index - 1] in " \t"):
            break
        out.append(char)
    return "".join(out).rstrip()


_YAML_KEY = re.compile(r"^([^\s:'\"]+|'[^']*'|\"[^\"]*\")\s*:(?:\s+|$)(.*)$")
_YAML_INLINE_KEY = re.compile(r"^([^\s:'\"]+|'[^']*'|\"[^\"]*\")\s*:(?:\s|$)(.*)$")
_YAML_BLOCK_MARKERS = {"|", ">", "|-", ">-", "|+", ">+"}


def _preprocess_yaml_lines(raw: list[str]) -> list[tuple[int, int, str]]:
    lines: list[tuple[int, int, str]] = []
    for number, line in enumerate(raw, 1):
        stripped = line.strip()
        if stripped in ("---", "...") and lines:
            break
        if stripped == "---":
            continue
        content = _strip_comment(line.expandtabs(2))
        if not content.strip():
            continue
        lines.append((number, len(content) - len(content.lstrip(" ")), content.strip()))
    return lines


class _YamlReader:
    def __init__(self, raw: list[str], lines: list[tuple[int, int, str]]) -> None:
        self.raw = raw
        self.lines = lines
        self.position = 0

    def block_scalar(self, indent: int, marker: str) -> str:
        parts: list[str] = []
        while self.position < len(self.lines) and self.lines[self.position][1] > indent:
            parts.append(self.raw[self.lines[self.position][0] - 1].strip())
            self.position += 1
        return ("\n" if marker.startswith("|") else " ").join(parts)

    def _is_seq_item(self, indent: int) -> bool:
        if self.position >= len(self.lines):
            return False
        _num, line_indent, text = self.lines[self.position]
        return line_indent == indent and (text.startswith("- ") or text == "-")

    def _parse_sequence(self, indent: int, start_line: int) -> YamlNode:
        node = YamlNode([], start_line)
        while self._is_seq_item(indent):
            item_number, _line_indent, item = self.lines[self.position]
            rest = item[1:].strip()
            node.lines[len(node.value)] = item_number
            node.value.append(self._parse_sequence_item(indent, item_number, rest))
        return node

    def _parse_sequence_item(self, indent: int, item_number: int, rest: str) -> Any:
        if not rest:
            self.position += 1
            if self.position < len(self.lines) and self.lines[self.position][1] > indent:
                return self.parse_block(self.lines[self.position][1]).value
            return None
        if _YAML_INLINE_KEY.match(rest) and not rest.startswith(("[", "{")):
            # A mapping that starts on the dash line: its keys are indented past the dash.
            self.lines[self.position] = (item_number, indent + 2, rest)
            return self.parse_block(indent + 2).value
        self.position += 1
        return _yaml_scalar(rest)

    def _parse_mapping_value(self, indent: int, rest: str) -> Any:
        if rest in _YAML_BLOCK_MARKERS:
            return self.block_scalar(indent, rest)
        if rest != "":
            return _yaml_scalar(rest)
        if self.position >= len(self.lines):
            return None
        next_indent = self.lines[self.position][1]
        if next_indent > indent:
            return self.parse_block(next_indent).value
        if next_indent == indent and self.lines[self.position][2].startswith("- "):
            return self.parse_block(indent).value
        return None

    def _parse_mapping(self, indent: int, start_line: int) -> YamlNode:
        node = YamlNode({}, start_line)
        while self.position < len(self.lines) and self.lines[self.position][1] == indent:
            key_number, _line_indent, content = self.lines[self.position]
            match = _YAML_KEY.match(content)
            self.position += 1
            if not match:
                continue
            key = _yaml_scalar(match.group(1))
            rest = match.group(2).strip()
            node.lines[key] = key_number
            node.value[key] = self._parse_mapping_value(indent, rest)
        return node

    def parse_block(self, indent: int) -> YamlNode:
        number, _line_indent, content = self.lines[self.position]
        if content.startswith("- ") or content == "-":
            return self._parse_sequence(indent, number)
        return self._parse_mapping(indent, number)


def parse_yaml(text: str) -> YamlNode | None:
    """Block mappings and sequences, scalars, flow lists and maps on one line, block scalars (`|`, `>`),
    comments and a document marker: what a workflow or a manifest uses. Returns None for an empty file."""
    raw = text.splitlines()
    lines = _preprocess_yaml_lines(raw)
    if not lines:
        return None
    reader = _YamlReader(raw, lines)
    try:
        return reader.parse_block(lines[0][1])
    except (IndexError, RecursionError):
        return None


def _yaml_get(value: Any, *keys: Any) -> Any:
    for key in keys:
        if isinstance(value, dict):
            value = value.get(key)
        elif isinstance(value, list) and isinstance(key, int) and key < len(value):
            value = value[key]
        else:
            return None
    return value


_TF_NESTED = re.compile(r"^\s*([a-z_]+)\s*\{")
_TF_ATTR = re.compile(r"^\s*([a-z_][a-z0-9_]*)\s*=\s*(.*)$")
_TF_REF = re.compile(
    r"\b(var|local|module|data)\.([A-Za-z_][\w-]*)"
    r"|\b([a-z][a-z0-9_]*_[a-z0-9_]+)\.([A-Za-z_][\w-]*)\b"
)
_TF_HEREDOC = re.compile(r"<<-?\s*([A-Z_]+)\s*$")


def _start_tf_block(path: str, number: int, stripped: str) -> TfBlock | None:
    match = _TF_BLOCK.match(stripped)
    if not match:
        return None
    kind, first, second = match.groups()
    name = second or "" if kind in ("resource", "data") else ""
    return TfBlock(kind, first or "", name, path, number, number)


def _record_tf_refs(block: TfBlock, stripped: str) -> None:
    for match in _TF_REF.finditer(stripped):
        if match.group(1):
            block.refs.add(f"{match.group(1)}.{match.group(2)}")
        elif match.group(3):
            block.refs.add(f"{match.group(3)}.{match.group(4)}")


def _record_tf_attr(
    block: TfBlock,
    depth: int,
    nested_name: str | None,
    stripped: str,
) -> None:
    attr = _TF_ATTR.match(stripped)
    if not attr:
        return
    if depth == 1:
        block.attrs[attr.group(1)] = _clip(attr.group(2), 200)
    elif depth == 2 and nested_name is not None:
        block.nested.setdefault(nested_name, {})[attr.group(1)] = _clip(attr.group(2), 200)


def _record_tf_body_line(
    block: TfBlock,
    depth: int,
    nested_name: str | None,
    stripped: str,
) -> tuple[str | None, str | None]:
    doc = _TF_HEREDOC.search(stripped)
    heredoc = doc.group(1) if doc else None
    ends_brace = stripped.rstrip().endswith("{")

    # A heredoc attribute (a policy document) is skipped with its body: its lines are not the block's wiring.
    if not doc and not ends_brace:
        _record_tf_attr(block, depth, nested_name, stripped)

    nested = _TF_NESTED.match(stripped)
    if nested and depth == 1 and ends_brace:
        nested_name = nested.group(1)
    _record_tf_refs(block, stripped)
    return nested_name, heredoc


def parse_terraform(path: str, text: str) -> list[TfBlock]:
    blocks: list[TfBlock] = []
    lines = text.splitlines()
    current: TfBlock | None = None
    depth = 0
    nested_name: str | None = None
    heredoc: str | None = None

    for number, line in enumerate(lines, 1):
        if heredoc is not None:
            if line.strip() == heredoc:
                heredoc = None
            continue
        stripped = re.sub(r"(#|//).*$", "", line)
        if current is None:
            current = _start_tf_block(path, number, stripped)
            depth = 1 if current is not None else 0
            nested_name = None
            continue
        nested_name, heredoc = _record_tf_body_line(current, depth, nested_name, stripped)
        depth += stripped.count("{") - stripped.count("}")
        if depth <= 0:
            current.end = number
            blocks.append(current)
            current = None

    if current is not None:
        current.end = len(lines)
        blocks.append(current)
    return blocks
