//! Path helpers for resource discovery: `resolvePath` against `$HOME`, `canonicalizePath`, BOM stripping. Node's
//! `path` itself is [`jsrt::path`].

use std::path::Path;

/// `resolvePath` from utils/paths.ts against `$HOME`, trimming the input first when `trim` is set.
pub fn resolve_path(input: &str, base: &str, trim: bool) -> String {
    let input = if trim { input.trim() } else { input };
    jsrt::path::resolve_path(input, base, &std::env::var("HOME").unwrap_or_default())
}

/// `canonicalizePath`: realpath, or the input when it cannot be resolved.
pub fn canonicalize(path: &str) -> String {
    std::fs::canonicalize(Path::new(path))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Strip a leading UTF-8 byte order mark.
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}
