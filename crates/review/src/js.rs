//! JavaScript semantics the ported autoreview code depends on that `jsrt` does not hold: Node's `path.join`, numbers
//! that serialize like JSON.stringify writes them, and `Math.min(max, Math.max(min, value))`.

use serde::{Deserialize, Serialize, Serializer};

/// Node's `path.posix.join`: the non-empty parts joined with `/`, then normalized (`.`, `..` and repeated slashes
/// resolved; a trailing slash kept); `.` when nothing is left.
pub fn path_join(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        return ".".into();
    }
    let absolute = joined.starts_with('/');
    let trailing = joined.ends_with('/');
    let mut out: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|last| *last != "..") {
                    out.pop();
                } else if !absolute {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let mut path = out.join("/");
    if path.is_empty() && !absolute {
        path = ".".into();
    }
    if trailing && !path.is_empty() && !(absolute && out.is_empty()) {
        path.push('/');
    }
    if absolute { format!("/{path}") } else { path }
}

/// A JavaScript number: written by JSON.stringify as an integer when it is one.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Deserialize)]
#[serde(transparent)]
pub struct Num(pub f64);

impl Serialize for Num {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        jsrt::json::number(self.0).serialize(serializer)
    }
}

/// `Math.min(max, Math.max(min, value))`.
pub fn clamp(value: f64, min: f64, max: f64) -> f64 {
    max.min(min.max(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_js_path_join_normalizes_like_node() {
        // Literal values from node's path.posix.join.
        assert_eq!(path_join(&["/agent/", "autoreview"]), "/agent/autoreview");
        assert_eq!(path_join(&["/a/../b", "x"]), "/b/x");
        assert_eq!(path_join(&["", "mise"]), "mise");
        assert_eq!(path_join(&[".", "mise"]), "mise");
        assert_eq!(path_join(&["a/../..", "b"]), "../b");
        assert_eq!(path_join(&["/..", ".."]), "/");
        assert_eq!(path_join(&["a", ".."]), ".");
        assert_eq!(path_join(&["a/", ""]), "a/");
        assert_eq!(path_join(&["/", ""]), "/");
        assert_eq!(path_join(&["", ""]), ".");
        assert_eq!(path_join(&["x/./", "./"]), "x/");
    }
}
