pub use crate::objects::*;
use stitch::*;
use tui::{
    Component, Markdown, MarkdownOptions, MarkdownTheme, StyleFn, classify_paste,
    extract_image_paste_paths_from_text, extract_paste_paths_from_text, markdown_tokens, visible_width,
};

fn sty(open: u32, close: u32) -> StyleFn {
    Box::new(move |s| format!("\x1b[{open}m{s}\x1b[{close}m"))
}

fn plain_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: sty(36, 39),
        link: sty(34, 39),
        link_url: sty(90, 39),
        code: sty(33, 39),
        code_block: sty(32, 39),
        code_block_border: sty(90, 39),
        quote: sty(37, 39),
        quote_border: sty(90, 39),
        hr: sty(90, 39),
        list_bullet: sty(35, 39),
        bold: sty(1, 22),
        italic: sty(3, 23),
        strikethrough: sty(9, 29),
        underline: sty(4, 24),
        highlight_code: None,
        code_block_indent: None,
        underline_links: true,
        link_targets: false,
    }
}

endpoint! {
    fn fuzz_markdown_render(src: Str<512, MdText>, width: Range<4, 160>, pad_x: Range<0, 4>) {
        let mut md = Markdown {
            tokens: markdown_tokens(&src),
            padding_x: *pad_x as usize,
            padding_y: 0,
            theme: plain_theme(),
            style: None,
            options: MarkdownOptions::default(),
        };
        let lines = md.render(*width as usize);
        for line in &lines {
            let _ = visible_width(line);
        }
    }
}

endpoint! {
    fn fuzz_paste_attachments(text: Str<256, AnsiText>) {
        let _ = classify_paste(&text);
        let _ = extract_paste_paths_from_text(&text);
        let _ = extract_image_paste_paths_from_text(&text);
    }
}
