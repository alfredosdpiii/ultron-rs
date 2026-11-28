pub use crate::objects::*;
use std::time::Instant;
use stitch::*;
use tui::{
    FullscreenViewport, KeyContext, KeybindingsManager, Rect, ScrollView, Scrollbar, StdinBuffer, UserBindings,
    render_latex, tui_keybindings,
};

fn scrollbar_from_u8(v: u8) -> Scrollbar {
    match v % 3 {
        0 => Scrollbar::Hidden,
        1 => Scrollbar::Auto,
        _ => Scrollbar::Always,
    }
}

endpoint! {
    fn stdin_new() -> New<StdinBuf> {
        New::new(StdinBuffer::new())
    }
}

endpoint! {
    fn stdin_process_str(buf: Borrow<StdinBuf>, data: Str<128, AnsiText>) {
        let _ = buf.process(&data);
    }
}

endpoint! {
    fn stdin_process_bytes(buf: Borrow<StdinBuf>, data: Bytes<128>) {
        let _ = buf.process_bytes(&data);
    }
}

endpoint! {
    fn stdin_flush(buf: Borrow<StdinBuf>) {
        let _ = buf.flush();
        check(
            buf.buffered().is_empty(),
            "StdinBuffer::flush must leave buffer empty",
        );
    }
}

endpoint! {
    fn stdin_clear(buf: Borrow<StdinBuf>) {
        buf.clear();
        check(
            buf.buffered().is_empty(),
            "StdinBuffer::clear must leave buffer empty",
        );
    }
}

endpoint! {
    fn scroll_new(follow_end: bool, mode: u8) -> New<ScrollObj> {
        New::new(ScrollView::new(follow_end, scrollbar_from_u8(mode)))
    }
}

endpoint! {
    fn scroll_layout(view: Borrow<ScrollObj>, content_h: Range<0, 500>, viewport_h: Range<0, 200>) {
        view.update_layout(*content_h as usize, *viewport_h as usize);
        let max_top = view.content_height().saturating_sub(view.viewport_height());
        check(
            view.scroll_top() <= max_top,
            "ScrollView scroll_top exceeded max_top after update_layout",
        );
    }
}

endpoint! {
    fn scroll_move(view: Borrow<ScrollObj>, delta: Range<-200, 200>, target: Range<-20, 520>, disable_follow: bool) {
        let _ = view.scroll_by(*delta as i64);
        view.scroll_to(*target as i64, disable_follow);
        let max_top = view.content_height().saturating_sub(view.viewport_height());
        check(
            view.scroll_top() <= max_top,
            "ScrollView scroll_top exceeded max_top after scroll",
        );
    }
}

endpoint! {
    fn scroll_ends(view: Borrow<ScrollObj>, to_end: bool, active: bool) {
        view.set_scrollbar_active(active);
        if to_end {
            view.scroll_to_end();
        } else {
            view.scroll_to_start();
            check(view.scroll_top() == 0, "scroll_to_start must set scroll_top to 0");
        }
    }
}

endpoint! {
    fn viewport_new(follow_end: bool, mode: u8) -> New<ViewportObj> {
        New::new(FullscreenViewport::new(ScrollView::new(
            follow_end,
            scrollbar_from_u8(mode),
        )))
    }
}

endpoint! {
    fn viewport_layout_and_frame(
        vp: Borrow<ViewportObj>,
        doc: Borrow<AnsiDoc>,
        cols: Range<1, 160>,
        rows: Range<1, 60>,
    ) {
        let w = *cols as usize;
        let h = *rows as usize;
        vp.set_size(w, h);
        let lines: Vec<String> = doc.lines().map(String::from).collect();
        let rect = Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let rendered = vp.layout(lines, rect);
        let framed = vp.finish_frame(rendered, w, h);
        check(framed.len() <= h, "finish_frame returned more rows than height");
    }
}

endpoint! {
    fn viewport_input(vp: Borrow<ViewportObj>, data: Str<64, MouseSeq>) {
        let keys = KeybindingsManager::new(
            tui_keybindings(),
            UserBindings::default(),
            KeyContext::default(),
        );
        let _ = vp.handle_input(&data, &keys, Instant::now());
    }
}

endpoint! {
    fn viewport_reset(vp: Borrow<ViewportObj>, hide: bool) {
        if hide {
            vp.hide_layout();
        } else {
            vp.reset();
        }
    }
}

endpoint! {
    fn fuzz_latex(src: Str<256, MdText>, display: bool) {
        let _ = render_latex(&src, display);
    }
}
