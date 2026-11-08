//! `SplitView` (packages/tui components/split-view.ts): replays fixtures/split-view-parity.json, recorded by
//! `node crates/tui/fixtures/generate-split-view.mjs`.

use serde_json::Value;

#[test]
fn tui_split_view_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/split-view-parity.json")).unwrap();
    let lines = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().into())
            .collect()
    };
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 32);
    for case in cases {
        let width = case["width"].as_u64().unwrap() as usize;
        let open = case["open"].as_bool().unwrap();
        let (main_width, side_width) =
            tui::split_widths(width, open, 44usize.max((width as f64 * 0.4).round() as usize));
        assert_eq!(
            (main_width as u64, side_width as u64),
            (case["mainWidth"].as_u64().unwrap(), case["sideWidth"].as_u64().unwrap()),
            "{case}"
        );
        let main = lines(&fixture["mains"][case["main"].as_u64().unwrap() as usize]);
        let side = lines(&fixture["sides"][case["side"].as_u64().unwrap() as usize]);
        let got = tui::render_split(&main, &side, main_width, side_width, "\x1b[90m│\x1b[39m");
        assert_eq!(got, lines(&case["lines"]), "{width} open={open}");
    }
}

#[test]
fn tui_split_view_keeps_the_editor_cursor_marker() {
    let main = vec![format!("ab{}c", tui::CURSOR_MARKER)];
    let got = tui::render_split(&main, &["x".into()], 10, 5, "│");
    assert!(got[0].contains(tui::CURSOR_MARKER), "{:?}", got[0]);
}
