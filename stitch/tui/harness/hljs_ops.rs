pub use crate::objects::*;
use stitch::*;
use tui::{HighlightColors, highlight_code, highlight_lines, hljs};

fn plain_colors() -> HighlightColors {
    HighlightColors {
        comment: "\x1b[90m".to_string(),
        keyword: "\x1b[35m".to_string(),
        function: "\x1b[34m".to_string(),
        variable: "\x1b[37m".to_string(),
        string: "\x1b[32m".to_string(),
        number: "\x1b[33m".to_string(),
        r#type: "\x1b[36m".to_string(),
        operator: "\x1b[31m".to_string(),
        punctuation: "\x1b[37m".to_string(),
        inserted: "\x1b[32m".to_string(),
        deleted: "\x1b[31m".to_string(),
    }
}

const LANGS: &[Option<&str>] = &[
    Some("rust"),
    Some("python"),
    Some("ts"),
    Some("bash"),
    Some("json"),
    None,
];

endpoint! {
    fn fuzz_highlight_code(src: Str<512, MdText>, lang_idx: Range<0, 5>) {
        let colors = plain_colors();
        let lang = LANGS[*lang_idx as usize];
        let _ = highlight_code(&src, lang, &colors);
        let _ = highlight_lines(&src, lang, &colors);
    }
}

endpoint! {
    fn fuzz_hljs_js_regex(pattern: Str<256, AnsiText>, ci: bool) {
        let _ = hljs::js_regex(&pattern, ci);
    }
}
