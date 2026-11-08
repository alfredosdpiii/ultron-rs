//! Node's POSIX `path` (`normalize`, `join`, `resolve`) and the coding agent's `normalizePath`/`resolvePath`
//! (utils/paths.ts), on strings, lexically. The home directory and base directory are parameters.

fn segments(path: &str, allow_above_root: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if allow_above_root {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// `path.normalize`: `.` and `..` removed (`..` above the root dropped), a trailing slash kept.
pub fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut s = segments(path, !absolute);
    if s.is_empty() {
        if absolute {
            return "/".to_string();
        }
        return if trailing { "./" } else { "." }.to_string();
    }
    if trailing {
        s.push('/');
    }
    if absolute { format!("/{s}") } else { s }
}

pub fn join(parts: &[&str]) -> String {
    let joined: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if joined.is_empty() {
        return ".".to_string();
    }
    normalize(&joined.join("/"))
}

/// `path.resolve(base, path)`: normalized, no trailing slash. Lexical: with a relative `base` and `path` the
/// result stays relative, where Node would prefix the process cwd.
pub fn resolve(base: &str, path: &str) -> String {
    let full = if path.starts_with('/') || base.is_empty() {
        path.to_string()
    } else {
        format!("{base}/{path}")
    };
    if full.starts_with('/') {
        return format!("/{}", segments(&full, false));
    }
    let relative = segments(&full, true);
    if relative.is_empty() { ".".to_string() } else { relative }
}

/// [`resolve`] from `/` for a filesystem path: absolute, `.` and `..` folded lexically.
pub fn absolute(path: &std::path::Path) -> std::path::PathBuf {
    resolve("/", &path.to_string_lossy()).into()
}

pub fn dirname(path: &str) -> String {
    let b = path.as_bytes();
    if b.is_empty() {
        return ".".into();
    }
    let has_root = b[0] == b'/';
    let mut end = None;
    let mut matched_slash = true;
    for i in (1..b.len()).rev() {
        if b[i] == b'/' {
            if !matched_slash {
                end = Some(i);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None => (if has_root { "/" } else { "." }).into(),
        Some(1) if has_root => "//".into(),
        Some(e) => path[..e].into(),
    }
}

pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// `path.relative(from, to)`, both resolved against `/`.
pub fn relative(from: &str, to: &str) -> String {
    let (from, to) = (resolve("/", from), resolve("/", to));
    let a: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out: Vec<&str> = vec![".."; a.len() - common];
    out.extend(&b[common..]);
    out.join("/")
}

/// `fileURLToPath` on POSIX: an empty or `localhost` host, no query or fragment, percent-decoded.
fn file_url_path(rest: &str) -> String {
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    crate::uri::decode(path.split(['?', '#']).next().unwrap_or(path))
}

/// `normalizePath`: `~` and `~/x` expand against `home`; a `file://` URL becomes its path.
pub fn normalize_path(input: &str, home: &str) -> String {
    if input == "~" {
        return home.to_string();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return join(&[home, rest]);
    }
    if let Some(rest) = input.strip_prefix("file://") {
        return file_url_path(rest);
    }
    input.to_string()
}

/// `resolvePath(input, base)`: `normalizePath` on both, then `path.resolve`. `base` resolves to an absolute path.
pub fn resolve_path(input: &str, base: &str, home: &str) -> String {
    resolve(&normalize_path(base, home), &normalize_path(input, home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_follows_node_posix_semantics() {
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("a/../"), "./");
        assert_eq!(normalize("/a/./b/../c/"), "/a/c/");
        assert_eq!(normalize("../a/.."), "..");
        assert_eq!(normalize("/../a"), "/a");
        assert_eq!(join(&["/a", "", "b/", "../c"]), "/a/c");
        assert_eq!(join(&["", ""]), ".");
        assert_eq!(join(&["/agent/", "autoreview"]), "/agent/autoreview");
        assert_eq!(join(&[".", "mise"]), "mise");
        assert_eq!(join(&["a/../..", "b"]), "../b");
        assert_eq!(join(&["/..", ".."]), "/");
        assert_eq!(join(&["a", ".."]), ".");
        assert_eq!(join(&["a/", ""]), "a/");
        assert_eq!(join(&["x/./", "./"]), "x/");
        assert_eq!(resolve("/a/b", "../c/./d/"), "/a/c/d");
        assert_eq!(resolve("/a", "/x/../y"), "/y");
        assert_eq!(resolve("/", "../.."), "/");
        assert_eq!(resolve(".", "a/../../b/"), "../b");
        assert_eq!(dirname("/a/b/c/"), "/a/b");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("a"), ".");
        assert_eq!(dirname("//a"), "//");
        assert_eq!(basename("/a/b/SKILL.md"), "SKILL.md");
        assert_eq!(basename("/a/b/"), "b");
        assert_eq!(basename("/"), "");
        assert_eq!(relative("/a/b", "/a/c/d"), "../c/d");
        assert_eq!(relative("/a/b/", "/a/b"), "");
    }

    #[test]
    fn normalize_path_expands_home_and_file_urls() {
        assert_eq!(normalize_path("~", "/h"), "/h");
        assert_eq!(normalize_path("~/x/", "/h"), "/h/x/");
        assert_eq!(normalize_path("~x", "/h"), "~x");
        assert_eq!(normalize_path("file:///a%20b/c?q=1#f", "/h"), "/a b/c");
        assert_eq!(normalize_path("file://localhost/a", "/h"), "/a");
        assert_eq!(resolve_path("x/../y", "~/w", "/h"), "/h/w/y");
        assert_eq!(resolve_path("/abs/./p", "/w", "/h"), "/abs/p");
    }
}
