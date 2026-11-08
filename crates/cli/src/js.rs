//! JavaScript semantics the ported subcommands print with that `jsrt` does not hold: `String.prototype.replace` with a
//! string pattern.

/// `haystack.replace(pattern, replacement)` with a string pattern: the first occurrence only, and the replacement's
/// `$$`, `$&`, `` $` `` and `$'` patterns expanded.
pub fn replace_first(haystack: &str, pattern: &str, replacement: &str) -> String {
    let Some(at) = haystack.find(pattern) else {
        return haystack.to_string();
    };
    let before = &haystack[..at];
    let after = &haystack[at + pattern.len()..];
    let mut out = String::with_capacity(haystack.len() + replacement.len());
    out.push_str(before);
    let mut chars = replacement.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().map(|(_, n)| *n) {
            Some('$') => {
                out.push('$');
                chars.next();
            }
            Some('&') => {
                out.push_str(pattern);
                chars.next();
            }
            Some('`') => {
                out.push_str(before);
                chars.next();
            }
            Some('\'') => {
                out.push_str(after);
                chars.next();
            }
            _ => out.push('$'),
        }
    }
    out.push_str(after);
    out
}
