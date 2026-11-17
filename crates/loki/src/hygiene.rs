//! Code hygiene: the vendored codehygiene model (`vendor/codehygiene`, see its UPSTREAM.md) run over the files a cell
//! wrote. It reports as advisories the lines the change added, never the debt a file already had: upstream gates on
//! the whole file, which flags most reviewed code (long lines and four indent levels count as squiggles).
//!
//! The model is NumPy code, so it runs in a Python that has NumPy: Loki's interpreter, else `uv run --offline` when
//! NumPy is in uv's cache. Nothing is downloaded; without NumPy the check says once that it did not run.
// ponytail: a Python + NumPy subprocess per check (~0.3 s plus ~0.4 s per 100 lines); port the forward pass to Rust
// if the NumPy requirement or the latency matters.

use crate::git::{git_output, text_at};
use crate::proc::{self, Deadline, RunError};
use crate::{py, resolve};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::Path;

const SOURCE: &str = include_str!("../vendor/codehygiene/__init__.py");
const WEIGHTS: &[u8] = include_bytes!("../vendor/codehygiene/weights.npz");

/// `off`, `on` (the default kinds) or `all` (every kind); over the global `loki.hygiene` setting.
pub const HYGIENE_ENV: &str = "ULTRON_LOKI_HYGIENE";

/// The kinds reported by default. `dense_line`, `deep_nesting` and `cramped_layout` fire on formatter-clean code
/// (lines past ~96 columns, four indent levels), and `poor_naming` on long test names; `all` adds them.
pub const DEFAULT_KINDS: [&str; 4] = ["swallowed_error", "noisy_comment", "duplicate_block", "special_casing"];

/// The languages upstream's circuits are written for.
const SUFFIXES: [&str; 19] = [
    ".py", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".rs", ".go", ".c", ".h", ".cc", ".cpp", ".cxx", ".hpp",
    ".zig", ".java", ".cs", ".lua",
];
const SECONDS: f64 = 20.0;
const MAX_NOTES: usize = 10;
const SNIPPET_CHARS: usize = 80;
/// The driver's exit when the model itself failed (3: no NumPy in that Python).
const FAILED: i32 = 4;

/// Reads `{source, weights, texts}` on stdin and prints, per text, `[[line, kind, message], ...]`.
const DRIVER: &str = r#"
import io, json, sys, types
try:
    import numpy
except ImportError:
    raise SystemExit(3)
try:
    request = json.load(sys.stdin)
    module = types.ModuleType("codehygiene")
    sys.modules["codehygiene"] = module
    exec(compile(request["source"], "codehygiene/__init__.py", "exec"), module.__dict__)
    with numpy.load(io.BytesIO(bytes.fromhex(request["weights"]))) as archive:
        weights = module.unpack_ternary_archive(archive["trits"], archive["vecs"], str(archive["meta"].item()))
    found = [
        [[s.line, s.kind, s.message] for s in module.score(text, weights=weights).squiggles]
        for text in request["texts"]
    ]
except Exception as error:
    sys.stderr.write(f"{type(error).__name__}: {error}\n")
    raise SystemExit(4)
json.dump(found, sys.stdout)
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Off,
    On,
    All,
}

/// `ULTRON_LOKI_HYGIENE`, else the saved `loki.hygiene` setting, else on.
pub fn level(env: Option<&str>, settings: &Map<String, Value>) -> Level {
    let named = env
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .or_else(|| settings.get("hygiene").and_then(Value::as_str).map(String::from));
    match named.as_deref() {
        Some("all") => Level::All,
        Some("off" | "0" | "false" | "no" | "none") => Level::Off,
        _ => Level::On,
    }
}

/// One line the model flags: its 1-based line, upstream's kind and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Squiggle {
    pub line: usize,
    pub kind: String,
    pub message: String,
}

/// The Pythons to try, in order: Loki's own, then uv's with NumPy from its cache.
fn launchers() -> Vec<Vec<String>> {
    let mut found = vec![vec![crate::pyast::interpreter()]];
    if let Some(uv) = proc::which("uv") {
        let args = ["run", "-q", "--offline", "--no-project", "--with", "numpy", "python"];
        found.push(std::iter::once(uv).chain(args.map(String::from)).collect());
    }
    found
}

fn parse(stdout: &[u8]) -> Option<Vec<Vec<Squiggle>>> {
    let reply: Vec<Vec<(usize, String, String)>> = serde_json::from_slice(stdout).ok()?;
    let squiggle = |(line, kind, message)| Squiggle { line, kind, message };
    Some(
        reply
            .into_iter()
            .map(|file| file.into_iter().map(squiggle).collect())
            .collect(),
    )
}

/// The model's squiggles for each text, or why it could not run.
pub fn score(texts: &[&str], deadline: Deadline) -> Result<Vec<Vec<Squiggle>>, String> {
    let weights: String = WEIGHTS.iter().map(|byte| format!("{byte:02x}")).collect();
    let request = json!({"source": SOURCE, "weights": weights, "texts": texts}).to_string();
    // One BLAS thread: the check runs in the background beside the session's own work.
    let env = [("OMP_NUM_THREADS", "1"), ("OPENBLAS_NUM_THREADS", "1")];
    for mut command in launchers() {
        command.extend(["-I", "-c", DRIVER].map(String::from));
        let timeout = deadline.timeout().map_err(|_| "timed out".to_string())?;
        match proc::run(&command, &std::env::temp_dir(), Some(request.as_bytes()), &env, timeout) {
            Ok(out) if out.code == 0 => return parse(&out.stdout).ok_or_else(|| "unreadable reply".to_string()),
            Ok(out) if out.code == FAILED => {
                let stderr = proc::text(&out.stderr);
                return Err(stderr
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("failed")
                    .chars()
                    .take(200)
                    .collect());
            }
            Err(RunError::Timeout(_)) => return Err("timed out".into()),
            // No NumPy there (the driver's exit 3, or uv without it cached), or no such launcher: try the next.
            Ok(_) | Err(RunError::Os(..)) => {}
        }
    }
    Err(
        "needs NumPy, which neither Loki's Python nor uv's cache has (`uv run --with numpy python -c pass` fetches it once)"
            .into(),
    )
}

/// The 1-based lines of `after` that are not in `before`.
fn added_lines(before: &str, after: &str) -> HashSet<usize> {
    let (old, new) = (py::splitlines(before, false), py::splitlines(after, false));
    py::opcodes(&old, &new)
        .into_iter()
        .filter(|(tag, ..)| *tag != "equal")
        .flat_map(|(_, _, _, from, to)| from + 1..=to)
        .collect()
}

/// The advisory lines for `name`: the squiggles of `level`'s kinds on lines the change from `before` to `after`
/// added. Each carries the start of its line, so two findings of one kind in a file stay distinct to the host.
pub fn notes(name: &str, before: &str, after: &str, squiggles: &[Squiggle], level: Level) -> Vec<String> {
    let added = added_lines(before, after);
    squiggles
        .iter()
        .filter(|s| level == Level::All || (level == Level::On && DEFAULT_KINDS.contains(&s.kind.as_str())))
        .filter(|s| added.contains(&s.line))
        .map(|s| {
            let line = crate::hook::line_text(after, s.line as i64);
            let snippet: String = line.chars().take(SNIPPET_CHARS).collect();
            let kind = s.kind.replace('_', "-");
            format!("{name}:{}: loki/hygiene-{kind}: {}: `{snippet}`", s.line, s.message)
        })
        .collect()
}

/// Whether hygiene reads `name` (repository-relative): a language the model knows, outside vendored and generated code.
fn checked(name: &str) -> bool {
    crate::rules::suffix(name).is_some_and(|suffix| SUFFIXES.contains(&suffix.as_str()))
        && crate::slop::slop_included(name)
}

/// Hygiene advisories for the written `files` (repository-relative) against their committed text, or one
/// `NOT CHECKED hygiene: ...` line when the model could not run.
pub fn written(root: &Path, files: &[String], level: Level) -> Vec<String> {
    against(root, files, "HEAD", level)
}

/// Files the working tree adds or modifies against `base`, at most this many, are read by [`changed`].
const MAX_CHANGED_FILES: usize = 40;

/// Hygiene advisories for every line the working tree adds against `base` (a revision): the whole change, where
/// [`written`] sees one cell's files against HEAD and so nothing of what is already committed.
pub fn changed(root: &Path, base: &str, level: Level) -> Vec<String> {
    if level == Level::Off {
        return vec![];
    }
    let arguments = ["diff", "--name-only", "-z", "--diff-filter=AM", base, "--"];
    let listed = match git_output(&resolve(root), &arguments, Deadline::after(SECONDS), None) {
        Ok(listed) => listed,
        Err(error) => return vec![format!("NOT CHECKED hygiene: {}", error.message())],
    };
    let files: Vec<String> = String::from_utf8_lossy(&listed)
        .split('\0')
        .filter(|name| !name.is_empty())
        .take(MAX_CHANGED_FILES)
        .map(String::from)
        .collect();
    against(root, &files, base, level)
}

fn against(root: &Path, files: &[String], base: &str, level: Level) -> Vec<String> {
    if level == Level::Off {
        return vec![];
    }
    let root = resolve(root);
    let sources: Vec<(&String, String)> = files
        .iter()
        .filter(|name| checked(name))
        .filter_map(|name| proc::read_text(&root.join(name)).ok().map(|text| (name, text)))
        .collect();
    if sources.is_empty() {
        return vec![];
    }
    let not_checked = |reason: &str| vec![format!("NOT CHECKED hygiene: {reason}")];
    let deadline = Deadline::after(SECONDS);
    let texts: Vec<&str> = sources.iter().map(|(_, text)| text.as_str()).collect();
    let scored = match score(&texts, deadline) {
        Ok(scored) => scored,
        Err(reason) => return not_checked(&reason),
    };
    let mut found = Vec::new();
    for ((name, after), squiggles) in sources.iter().zip(scored) {
        if squiggles.is_empty() {
            continue;
        }
        match text_at(&root, base, name, deadline) {
            Ok(before) => found.extend(notes(name, &before, after, &squiggles, level)),
            Err(error) => return not_checked(error.message()),
        }
    }
    if found.len() > MAX_NOTES {
        let more = found.len() - MAX_NOTES;
        found.truncate(MAX_NOTES);
        found.push(format!("... {more} more hygiene notes"));
    }
    found
}
