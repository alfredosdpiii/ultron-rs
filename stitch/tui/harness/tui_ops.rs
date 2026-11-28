pub use crate::objects::*;
use stitch::*;
use tui::{
    lex, osc8_link_at_column, parse_sgr_mouse, plain_url_at_column, slice_by_column, slice_with_width,
    strip_terminal_sequences, truncate_to_width, visible_width, wrap_text_with_ansi,
};

endpoint! {
    fn make_doc(text: Str<256, AnsiText>) -> New<AnsiDoc> {
        New::new(text.to_string())
    }
}

endpoint! {
    fn append_doc(doc: Borrow<AnsiDoc>, chunk: Str<128, AnsiText>) {
        if doc.len() < 4096 {
            doc.push_str(&chunk);
        }
    }
}

endpoint! {
    fn fuzz_wrap_and_strip(doc: Borrow<AnsiDoc>, width: Range<1, 160>) {
        let lines = wrap_text_with_ansi(&doc, *width as usize);
        let stripped = strip_terminal_sequences(&doc);
        let stripped_twice = strip_terminal_sequences(&stripped);
        check(
            stripped == stripped_twice,
            "strip_terminal_sequences must be idempotent",
        );
        for line in &lines {
            let _ = visible_width(line);
        }
    }
}

endpoint! {
    fn fuzz_truncate(doc: Borrow<AnsiDoc>, max_width: Range<0, 120>, pad: bool) {
        let out = truncate_to_width(&doc, *max_width as usize, "…", pad);
        let w = visible_width(&out);
        if *max_width >= 1 {
            check(
                w <= *max_width as usize,
                "truncate_to_width exceeded max_width",
            );
        }
    }
}

endpoint! {
    fn fuzz_slice(doc: Borrow<AnsiDoc>, start: Range<0, 120>, length: Range<0, 120>, strict: bool) {
        let (sliced, reported) = slice_with_width(&doc, *start as usize, *length as usize, strict);
        check(
            visible_width(&sliced) == reported,
            "slice_with_width reported width mismatch",
        );
        let col_sliced = slice_by_column(&doc, *start as usize, *length as usize);
        let _ = visible_width(&col_sliced);
    }
}

endpoint! {
    fn fuzz_links(doc: Borrow<AnsiDoc>, col: Range<0, 160>) {
        let _ = osc8_link_at_column(&doc, *col as usize);
        let _ = plain_url_at_column(&doc, *col as usize);
    }
}

endpoint! {
    fn fuzz_markdown_lex(src: Str<512, MdText>) {
        let _ = lex(&src);
    }
}

endpoint! {
    fn fuzz_sgr_mouse(seq: Str<64, AnsiText>) {
        let _ = parse_sgr_mouse(&seq);
    }
}
