//! Path helpers for resource discovery: `resolvePath` against a home directory, `canonicalizePath`, BOM stripping.
//! Node's `path` itself is [`jsrt::path`].

use std::path::Path;

/// `resolvePath` from utils/paths.ts (`~` names `home`), trimming the input first when `trim` is set.
pub fn resolve_path(input: &str, base: &str, home: &str, trim: bool) -> String {
    let input = if trim { input.trim() } else { input };
    jsrt::path::resolve_path(input, base, home)
}

/// `canonicalizePath`: realpath, or the input when it cannot be resolved.
pub fn canonicalize(path: &str) -> String {
    std::fs::canonicalize(Path::new(path))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}
