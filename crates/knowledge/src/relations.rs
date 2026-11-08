//! Relations across files, read on demand for a task: which code reads which configuration key, which handler
//! serves which route, which code touches which table, which test exercises which file.
//!
//! The schema is small and closed ([`Kind`]), versioned by [`RELATIONS_VERSION`], and grows only when a recorded
//! failure needs a distinction (a just-in-time ontology: built when a task asks, kept with its evidence, reused).
//! Each relation passes three rungs, cheapest first:
//!
//! 1. The proposer ([`propose`]): plain code over one blob's text. A pattern that cannot mean anything else
//!    (`std::env::var("HOME")`, `@app.get("/users")`, SQL in a `.sql` file) is settled here (`certain`); a
//!    pattern that only looks like one (`config.get("x.y")`, a lowercase `from users` in a string) is proposed.
//! 2. Pruning ([`prune`]): only the kinds the task asked for; a test import must name a file of the same tree;
//!    duplicates and relations the judge refused are dropped. No model is asked about what plain code can drop.
//! 3. The judge (`judge.decide`, asked by the REPL client): one yes/no per proposed relation, over its lines.
//!
//! What the proposer reads depends on the blob alone, so proposals and verdicts are cached by blob and
//! [`RELATIONS_VERSION`]: the next task over the same files asks nothing again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::extract::vendored;

/// The version of the proposer's rules and of the schema of kinds. Bump it whenever what is proposed changes: every
/// blob is then read again, and its earlier proposals and verdicts are dropped.
pub const RELATIONS_VERSION: i64 = 1;

/// Proposals kept per blob, in order: a bound on generated files.
pub const MAX_PER_BLOB: usize = 300;

/// Lines of context around a proposal, each way, and the characters of context and quote kept.
const CONTEXT_LINES: usize = 3;
const CONTEXT_CHARS: usize = 800;
const QUOTE_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    /// Code → a configuration key (an environment variable, a setting).
    ReadsConfig,
    /// A handler → the route it serves (`GET /users`).
    ServesRoute,
    /// Code → a database table it reads, writes or defines.
    TouchesTable,
    /// A test file → a file of the repository it imports.
    Tests,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::ReadsConfig, Kind::ServesRoute, Kind::TouchesTable, Kind::Tests];

    pub fn name(self) -> &'static str {
        match self {
            Kind::ReadsConfig => "reads_config",
            Kind::ServesRoute => "serves_route",
            Kind::TouchesTable => "touches_table",
            Kind::Tests => "tests",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// The relationship type of a graph mirror (Neo4j).
    pub fn edge(self) -> &'static str {
        match self {
            Kind::ReadsConfig => "READS_CONFIG",
            Kind::ServesRoute => "SERVES_ROUTE",
            Kind::TouchesTable => "TOUCHES_TABLE",
            Kind::Tests => "TESTS",
        }
    }

    /// The node label of a relation's object in a graph mirror; the subject is always `Code`.
    pub fn object_label(self) -> &'static str {
        match self {
            Kind::ReadsConfig => "ConfigKey",
            Kind::ServesRoute => "Route",
            Kind::TouchesTable => "Table",
            Kind::Tests => "Code",
        }
    }

    /// The yes/no question the judge is asked of a proposed relation, over its lines.
    pub fn question(self, subject: &str, object: &str) -> String {
        match self {
            Kind::ReadsConfig => format!(
                "Does `{subject}` read `{object}` as a configuration value here (an environment variable, a \
                 setting or a config entry), not some other lookup that only looks like one?"
            ),
            Kind::ServesRoute => format!("Is `{subject}` the handler that serves the HTTP route `{object}` here?"),
            Kind::TouchesTable => format!(
                "Is `{object}` a database table that `{subject}` reads, writes or defines through the SQL here, \
                 not a word in prose or a non-SQL string?"
            ),
            Kind::Tests => format!("Does the test file `{subject}` exercise code of `{object}`?"),
        }
    }
}

/// A relation the proposer read from one blob. It depends on the blob's text only: the file's path is joined in
/// when it is read back ([`Relation`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposed {
    pub kind: Kind,
    /// The definition it is made from (a function, method or class name); empty for the file itself.
    pub subject: String,
    /// What it relates to: the key, `METHOD /route`, the table, or (for a test) the module as imported.
    pub object: String,
    pub line: i64,
    /// The line, trimmed.
    pub quote: String,
    /// The lines around it, for the judge.
    pub context: String,
    /// Settled by plain code: nothing else could mean this.
    pub certain: bool,
}

/// A relation of one file at one revision, as the store keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct Relation {
    pub id: i64,
    pub kind: Kind,
    /// `path::definition`, or the path for the file itself.
    pub subject: String,
    pub object: String,
    pub path: String,
    pub blob: String,
    pub line: i64,
    pub quote: String,
    pub context: String,
    /// `accepted`, `rejected`, `review` (the judge was not sure) or `proposed` (not judged yet).
    pub status: String,
    /// `code` (the proposer or pruning settled it) or the judge backend that answered.
    pub checked_by: Option<String>,
    pub rung: Option<String>,
    pub model: Option<String>,
    pub confidence: Option<f64>,
}

impl Relation {
    /// Where it was read: `repo://<path>#L<line>` (the file at the blob of this revision).
    pub fn evidence(&self) -> String {
        format!("repo://{}#L{}", self.path, self.line)
    }
}

/// Whether the proposer reads `path`: source and SQL outside vendored and built trees.
pub fn scanned(path: &str) -> bool {
    if vendored(path) {
        return false;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name.rsplit_once('.').map_or("", |(_, extension)| extension);
    matches!(
        extension,
        "rs" | "py" | "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" | "sql"
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Rust,
    Python,
    Script,
    Sql,
}

fn source(path: &str) -> Source {
    let extension = path.rsplit_once('.').map_or("", |(_, extension)| extension);
    match extension {
        "rs" => Source::Rust,
        "py" => Source::Python,
        "sql" => Source::Sql,
        _ => Source::Script,
    }
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid built-in pattern")
}

/// A definition line: `fn`, `def`, `function`, `class`, a TypeScript method or arrow function.
static DEFINITION: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^\s*(?:export\s+)?(?:default\s+)?(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?(?:fn|def|function|class)\s+([A-Za-z_][A-Za-z0-9_]*)",
    )
});
static ARROW: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^\s*(?:export\s+)?(?:const|let)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:\(|function)")
});
static METHOD: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^\s+(?:(?:public|private|protected|static|async|override)\s+)*([A-Za-z_$][\w$]*)\s*\([^)]*\)\s*(?::\s*[^={]+)?\{\s*$",
    )
});
const NOT_METHODS: [&str; 7] = ["if", "for", "while", "switch", "catch", "return", "function"];

fn definition(line: &str) -> Option<String> {
    if let Some(found) = DEFINITION.captures(line).or_else(|| ARROW.captures(line)) {
        return Some(found[1].to_string());
    }
    let found = METHOD.captures(line)?;
    (!NOT_METHODS.contains(&&found[1])).then(|| found[1].to_string())
}

fn comment(line: &str, from: Source) -> bool {
    let line = line.trim_start();
    line.starts_with("//")
        || line.starts_with("/*")
        || line.starts_with('*')
        || (line.starts_with('#') && !line.starts_with("#[") && !line.starts_with("#!"))
        || (from == Source::Sql && line.starts_with("--"))
}

static ENV_CERTAIN: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r#"\benv::var(?:_os)?\(\s*"([A-Za-z_][A-Za-z0-9_]*)""#,
        r#"\benv!\(\s*"([A-Za-z_][A-Za-z0-9_]*)""#,
        r#"\bos\.environ(?:\.get)?\s*[\[(]\s*['"]([A-Za-z_][A-Za-z0-9_]*)['"]"#,
        r#"\bos\.getenv\(\s*['"]([A-Za-z_][A-Za-z0-9_]*)['"]"#,
        r#"\bprocess\.env\.([A-Za-z_][A-Za-z0-9_]*)"#,
        r#"\bprocess\.env\[\s*['"]([A-Za-z_][A-Za-z0-9_]*)['"]"#,
    ]
    .iter()
    .map(|pattern| regex(pattern))
    .collect()
});
/// A lookup that may be configuration: a `var("NAME")` closure, a settings or config object's getter.
static CONFIG_MAYBE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r#"\bvar\)?\(\s*"([A-Z][A-Z0-9_]{2,})""#,
        r#"\b(?:settings|config|cfg|conf|options)\w*(?:\(\))?\.get(?:_\w+)?\(\s*['"]([A-Za-z][\w.-]*)['"]"#,
    ]
    .iter()
    .map(|pattern| regex(pattern))
    .collect()
});

static PY_ROUTE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"^\s*@(\w+)\.(get|post|put|patch|delete|head|options|route|api_route)\(\s*['"]([^'"]+)['"]"#)
});
static PY_METHODS: LazyLock<Regex> = LazyLock::new(|| regex(r#"methods\s*=\s*\[\s*['"](\w+)['"]"#));
static NEST_ROUTE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"^\s*@(Get|Post|Put|Patch|Delete|Head|Options|All)\(\s*(?:['"]([^'"]*)['"])?\s*\)"#));
static EXPRESS_ROUTE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"\b(\w+)\.(get|post|put|patch|delete|all)\(\s*['"](/[^'"]*)['"]\s*,(.*)$"#));
static LAST_HANDLER: LazyLock<Regex> = LazyLock::new(|| regex(r"([A-Za-z_$][\w$.]*)\s*\)\s*;?\s*$"));
static AXUM_ROUTE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"\.route\(\s*"([^"]+)"\s*,\s*(get|post|put|patch|delete|head|options|any)\(\s*([A-Za-z_][\w:]*)\s*\)"#)
});
static RUST_ROUTE: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"^\s*#\[(get|post|put|patch|delete|head|options)\(\s*"([^"]+)""#));
static NEXT_HANDLER: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:public|private|protected|static|async)\s+)*(?:fn\s+|def\s+)?([A-Za-z_$][\w$]*)\s*[(<]",
    )
});
/// Receivers whose `get("/path", handler)` is a route.
const ROUTERS: [&str; 6] = ["app", "router", "server", "api", "routes", "bp"];

static SQL_TABLE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r#"(?i)\b(from|join)\s+[`"]?([A-Za-z_][\w]*(?:\.[A-Za-z_]\w*)?)"#,
        r#"(?i)\b(insert\s+into)\s+[`"]?([A-Za-z_][\w]*(?:\.[A-Za-z_]\w*)?)"#,
        r#"(?i)\b(update)\s+[`"]?([A-Za-z_][\w]*(?:\.[A-Za-z_]\w*)?)[`"]?\s+set\b"#,
        r#"(?i)\b((?:create|alter|drop|truncate)\s+table)\s+(?:if\s+(?:not\s+)?exists\s+)?[`"]?([A-Za-z_][\w]*(?:\.[A-Za-z_]\w*)?)"#,
    ]
    .iter()
    .map(|pattern| regex(pattern))
    .collect()
});
static TABLENAME: LazyLock<Regex> = LazyLock::new(|| regex(r#"\b__tablename__\s*=\s*['"](\w+)['"]"#));
/// A line of SQL by itself (a continuation of a multi-line SQL string).
static SQL_LINE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^(?:SELECT|FROM|JOIN|LEFT|RIGHT|INNER|OUTER|WHERE|INSERT|UPDATE|DELETE|CREATE|ALTER|DROP|VALUES|SET|RETURNING|ON|WITH|USING)\b",
    )
});
/// Words after `from` or `join` that are not tables.
const NOT_TABLES: [&str; 22] = [
    "a",
    "an",
    "the",
    "this",
    "that",
    "it",
    "each",
    "every",
    "all",
    "any",
    "select",
    "where",
    "values",
    "as",
    "on",
    "lateral",
    "unnest",
    "json_each",
    "generate_series",
    "dual",
    "here",
    "there",
];

static TEST_FILE: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?m)#\[test\]|#\[tokio::test|^\s*(?:async\s+)?def test_|^\s*(?:describe|it|test)\(|^\s*import pytest|^\s*import unittest",
    )
});
static RUST_USE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^\s*(?:pub\s+)?use\s+((?:::)?[A-Za-z_]\w*(?:::[A-Za-z_]\w*)*)"));
static PY_FROM: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s*from\s+(\.*[\w.]*)\s+import\s+\(?([^#]+)"));
static PY_IMPORT: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s*import\s+([\w.]+)"));
static SCRIPT_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?:\bfrom\s+|^\s*import\s+|\brequire\(\s*)['"]([^'"]+)['"]"#));

fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_string();
    }
    text.chars().take(chars).collect::<String>() + "..."
}

/// Everything the proposer reads in `text` (the file at `path`), in line order, at most [`MAX_PER_BLOB`].
pub fn propose(path: &str, text: &str) -> Vec<Proposed> {
    let from = source(path);
    let lines: Vec<&str> = text.lines().collect();
    let tests = from != Source::Sql && TEST_FILE.is_match(text);
    let mut current = String::new();
    let mut out = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(name) = definition(line) {
            current = name;
        }
        if comment(line, from) {
            continue;
        }
        let mut found: Vec<(Kind, String, String, bool)> = Vec::new();
        reads_config(line, &current, &mut found);
        serves_route(line, &lines[index + 1..], &mut found);
        touches_table(line, from, &current, &mut found);
        if tests {
            imports(line, from, &mut found);
        }
        for (kind, subject, object, certain) in found {
            let start = index.saturating_sub(CONTEXT_LINES);
            let end = (index + CONTEXT_LINES + 1).min(lines.len());
            out.push(Proposed {
                kind,
                subject,
                object,
                line: index as i64 + 1,
                quote: clip(line.trim(), QUOTE_CHARS),
                context: clip(&lines[start..end].join("\n"), CONTEXT_CHARS),
                certain,
            });
            if out.len() == MAX_PER_BLOB {
                return out;
            }
        }
    }
    out
}

fn reads_config(line: &str, current: &str, found: &mut Vec<(Kind, String, String, bool)>) {
    let mut seen = BTreeSet::new();
    for (patterns, certain) in [(&*ENV_CERTAIN, true), (&*CONFIG_MAYBE, false)] {
        for pattern in patterns.iter() {
            for captures in pattern.captures_iter(line) {
                let key = captures[1].to_string();
                if seen.insert(key.clone()) {
                    found.push((Kind::ReadsConfig, current.to_string(), key, certain));
                }
            }
        }
    }
}

/// The handler a decorator or attribute line stands over: the next definition within a few lines.
fn handler_below(below: &[&str]) -> Option<String> {
    below
        .iter()
        .take(6)
        .map(|line| line.trim_start())
        .find(|line| !line.starts_with('@') && !line.starts_with("#[") && !line.is_empty())
        .and_then(|line| NEXT_HANDLER.captures(line))
        .map(|found| found[1].to_string())
}

fn serves_route(line: &str, below: &[&str], found: &mut Vec<(Kind, String, String, bool)>) {
    let route = |method: &str, path: &str| {
        let method = match method.to_ascii_uppercase().as_str() {
            "ROUTE" | "API_ROUTE" | "ALL" => "ANY".to_string(),
            other => other.to_string(),
        };
        format!("{method} {path}")
    };
    // A decorator or attribute stands over its handler; the three forms never share a line.
    if let Some(captures) = PY_ROUTE.captures(line)
        && let Some(handler) = handler_below(below)
    {
        let method = PY_METHODS
            .captures(line)
            .map_or_else(|| captures[2].to_string(), |m| m[1].to_string());
        let receiver = captures[1].to_ascii_lowercase();
        let known = ROUTERS.iter().any(|name| receiver == *name || receiver.ends_with(name));
        found.push((Kind::ServesRoute, handler, route(&method, &captures[3]), known));
    }
    if let Some(captures) = NEST_ROUTE.captures(line)
        && let Some(handler) = handler_below(below)
    {
        let path = captures.get(2).map_or("", |m| m.as_str()).trim_start_matches('/');
        found.push((
            Kind::ServesRoute,
            handler,
            route(&captures[1], &format!("/{path}")),
            true,
        ));
    }
    if let Some(captures) = RUST_ROUTE.captures(line)
        && let Some(handler) = handler_below(below)
    {
        found.push((Kind::ServesRoute, handler, route(&captures[1], &captures[2]), true));
    }
    for captures in AXUM_ROUTE.captures_iter(line) {
        let handler = captures[3].rsplit("::").next().unwrap_or(&captures[3]).to_string();
        found.push((Kind::ServesRoute, handler, route(&captures[2], &captures[1]), true));
    }
    if let Some(captures) = EXPRESS_ROUTE.captures(line)
        && let Some(handler) = LAST_HANDLER.captures(&captures[4])
    {
        let handler = handler[1].rsplit('.').next().unwrap_or(&handler[1]).to_string();
        let receiver = captures[1].to_ascii_lowercase();
        let known = ROUTERS.iter().any(|name| receiver == *name || receiver.ends_with(name));
        found.push((Kind::ServesRoute, handler, route(&captures[2], &captures[3]), known));
    }
}

fn touches_table(line: &str, from: Source, current: &str, found: &mut Vec<(Kind, String, String, bool)>) {
    if let Some(captures) = TABLENAME.captures(line) {
        found.push((Kind::TouchesTable, current.to_string(), captures[1].to_string(), true));
    }
    let sql_line = SQL_LINE.is_match(line.trim_start());
    let mut seen = BTreeSet::new();
    for pattern in SQL_TABLE.iter() {
        for captures in pattern.captures_iter(line) {
            let (keyword, table) = (captures.get(1).expect("group 1"), captures[2].to_string());
            if NOT_TABLES.contains(&table.to_ascii_lowercase().as_str()) || !seen.insert(table.clone()) {
                continue;
            }
            let upper = keyword.as_str() == keyword.as_str().to_ascii_uppercase();
            let quotes = line[..keyword.start()]
                .chars()
                .filter(|c| matches!(c, '"' | '\'' | '`'))
                .count();
            let certain = match (from == Source::Sql, quotes % 2 == 1) {
                (true, _) => true,
                (false, quoted) if upper && (quoted || sql_line) => true,
                (false, true) => false,
                // Not SQL: `from x import y`, prose. Dropped here, no model asked.
                (false, false) => continue,
            };
            found.push((Kind::TouchesTable, current.to_string(), table, certain));
        }
    }
}

fn imports(line: &str, from: Source, found: &mut Vec<(Kind, String, String, bool)>) {
    let mut push = |module: String| found.push((Kind::Tests, String::new(), module, true));
    match from {
        Source::Rust => {
            if let Some(captures) = RUST_USE.captures(line) {
                push(captures[1].trim_start_matches("::").to_string());
            }
        }
        Source::Python => {
            if let Some(captures) = PY_FROM.captures(line) {
                let module = captures[1].to_string();
                for name in captures[2].split(',') {
                    let name = name
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .trim_matches(|c| c == '(' || c == ')');
                    if name.is_empty() || name == "*" {
                        continue;
                    }
                    let joined = if module.ends_with('.') {
                        format!("{module}{name}")
                    } else {
                        format!("{module}.{name}")
                    };
                    push(joined);
                }
            } else if let Some(captures) = PY_IMPORT.captures(line) {
                push(captures[1].to_string());
            }
        }
        Source::Script => {
            for captures in SCRIPT_IMPORT.captures_iter(line) {
                push(captures[1].to_string());
            }
        }
        Source::Sql => {}
    }
}

/// The repository file a test's import names, when it is one: Rust `use` paths (`crate::`, a workspace crate's
/// name), Python modules (dotted, relative), relative TypeScript and JavaScript specifiers.
pub fn resolve_import(path: &str, module: &str, paths: &BTreeSet<String>) -> Option<String> {
    match source(path) {
        Source::Rust => resolve_rust(path, module, paths),
        Source::Python => resolve_python(path, module, paths),
        Source::Script => resolve_script(path, module, paths),
        Source::Sql => None,
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn joined(dir: &str, rest: &str) -> String {
    if dir.is_empty() {
        rest.to_string()
    } else {
        format!("{dir}/{rest}")
    }
}

/// The deepest module file of `segments` under `root` (`<root>/a/b.rs`, `<root>/a/b/mod.rs`), else `fallback`.
fn rust_module(root: &str, segments: &[&str], fallback: Option<String>, paths: &BTreeSet<String>) -> Option<String> {
    let mut best = fallback.filter(|file| paths.contains(file));
    for depth in 1..=segments.len() {
        let stem = joined(root, &segments[..depth].join("/"));
        for file in [format!("{stem}.rs"), format!("{stem}/mod.rs")] {
            if paths.contains(&file) {
                best = Some(file);
            }
        }
    }
    best
}

fn resolve_rust(path: &str, module: &str, paths: &BTreeSet<String>) -> Option<String> {
    let segments: Vec<&str> = module.split("::").collect();
    let (first, rest) = segments.split_first()?;
    match *first {
        "crate" => {
            // The crate a file belongs to: `<dir>/src/` for its sources, `<dir>/tests/` for an integration test.
            let root = ["src", "tests"].iter().find_map(|dir| {
                let marker = format!("/{dir}/");
                path.find(&marker)
                    .map(|at| path[..at + marker.len() - 1].to_string())
                    .or_else(|| path.starts_with(&format!("{dir}/")).then(|| dir.to_string()))
            })?;
            rust_module(&root, rest, None, paths)
        }
        "self" | "super" | "std" | "core" | "alloc" => None,
        name => {
            let dashed = name.replace('_', "-");
            let lib = paths.iter().find(|file| {
                [name, dashed.as_str()].iter().any(|crate_dir| {
                    *file == &format!("{crate_dir}/src/lib.rs") || file.ends_with(&format!("/{crate_dir}/src/lib.rs"))
                })
            })?;
            let root = parent(lib).to_string();
            rust_module(&root, rest, Some(lib.clone()), paths)
        }
    }
}

fn resolve_python(path: &str, module: &str, paths: &BTreeSet<String>) -> Option<String> {
    let dots = module.len() - module.trim_start_matches('.').len();
    let rest = module.trim_start_matches('.');
    let candidates = |base: &str| [format!("{base}.py"), format!("{base}/__init__.py")];
    if dots > 0 {
        let mut dir = parent(path).to_string();
        for _ in 1..dots {
            dir = parent(&dir).to_string();
        }
        // `from .a import b`: `b` may be a module of `a`, else a name defined in `a`.
        let parts: Vec<&str> = rest.split('.').filter(|part| !part.is_empty()).collect();
        for keep in (0..=parts.len()).rev() {
            let base = joined(&dir, &parts[..keep].join("/"));
            if let Some(found) = candidates(&base).into_iter().find(|file| paths.contains(file)) {
                return Some(found);
            }
        }
        return None;
    }
    let parts: Vec<&str> = rest.split('.').collect();
    for keep in (1..=parts.len()).rev() {
        let suffix = parts[..keep].join("/");
        for file in candidates(&suffix) {
            if let Some(found) = paths.iter().find(|p| **p == file || p.ends_with(&format!("/{file}"))) {
                return Some(found.clone());
            }
        }
    }
    None
}

fn resolve_script(path: &str, module: &str, paths: &BTreeSet<String>) -> Option<String> {
    if !module.starts_with('.') {
        return None;
    }
    let mut parts: Vec<&str> = parent(path).split('/').filter(|part| !part.is_empty()).collect();
    for part in module.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    let base = parts.join("/");
    let mut candidates = vec![base.clone()];
    for extension in ["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
        candidates.push(format!("{base}.{extension}"));
    }
    for extension in ["ts", "tsx", "js"] {
        candidates.push(format!("{base}/index.{extension}"));
    }
    candidates.into_iter().find(|file| paths.contains(file))
}

/// What a task keeps of a revision's relations, and how many were dropped, by reason.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pruned {
    pub kept: Vec<Relation>,
    pub dropped: BTreeMap<&'static str, usize>,
}

/// The relations a task asked for (`kinds`, all when empty), pruned with plain code: another kind (`kind`), a
/// test's import that names no file of the tree (`unresolved`; a resolved one names the file), one the judge
/// refused (`rejected`), and the same (kind, subject, object) again (`duplicate`). `paths` is every file of the
/// tree.
pub fn prune(rows: Vec<Relation>, kinds: &[Kind], paths: &BTreeSet<String>) -> Pruned {
    let mut pruned = Pruned::default();
    let mut seen = BTreeSet::new();
    let mut drop = |reason: &'static str| *pruned.dropped.entry(reason).or_insert(0) += 1;
    let mut kept = Vec::new();
    for mut relation in rows {
        if !kinds.is_empty() && !kinds.contains(&relation.kind) {
            drop("kind");
            continue;
        }
        if relation.kind == Kind::Tests {
            match resolve_import(&relation.path, &relation.object, paths) {
                Some(file) if file != relation.path => relation.object = file,
                _ => {
                    drop("unresolved");
                    continue;
                }
            }
        }
        if relation.status == "rejected" {
            drop("rejected");
            continue;
        }
        if !seen.insert((relation.kind, relation.subject.clone(), relation.object.clone())) {
            drop("duplicate");
            continue;
        }
        kept.push(relation);
    }
    pruned.kept = kept;
    pruned
}
