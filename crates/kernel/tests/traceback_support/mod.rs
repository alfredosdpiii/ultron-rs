//! Shared by the tests that replay recorded runtime.py output (kernel's fixture, cli's print mode).

/// Python source line numbers (traceback `.py", line N` at any JSON escaping depth, warning `.py:N:`) become
/// `<line>`, so the recorded tracebacks and warnings do not pin runtime.py's layout.
pub fn without_line_numbers(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find(".py") {
        let after = &rest[i + 3..];
        let quotes = after.len() - after.trim_start_matches(['\\', '"']).len();
        let tail = &after[quotes..];
        out.push_str(&rest[..i + 3 + quotes]);
        rest = tail;
        let (marker, numbered) = match tail.strip_prefix(", line ") {
            Some(numbered) if quotes > 0 => (", line ", numbered),
            _ => (":", tail.strip_prefix(':').unwrap_or_default()),
        };
        let digits = numbered.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && (marker != ":" || numbered[digits..].starts_with(':')) {
            out.push_str(marker);
            out.push_str("<line>");
            rest = &numbered[digits..];
        }
    }
    out.push_str(rest);
    out
}
