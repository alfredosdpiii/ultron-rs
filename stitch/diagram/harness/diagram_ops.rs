pub use crate::objects::*;
use stitch::*;

fn default_palette() -> diagram::svg::DiagramPalette {
    let json = theme::builtin_theme_json("dark").expect("builtin dark theme");
    diagram::svg::palette_from_theme(&json, "dark").expect("dark palette")
}

endpoint! {
    fn diagram_flatten_and_contrast(svg: Str<1024, SvgDoc>) {
        let palette = default_palette();
        let flat = diagram::svg::flatten_svg(&svg, &palette);
        let _ = diagram::contrast::label_backdrops(&flat, &palette);
        let _ = diagram::contrast::ensure_contrast(&flat, &palette);
    }
}

endpoint! {
    fn diagram_parse_rgb(color: Str<128, ColorText>) {
        if let Some(rgb) = diagram::contrast::parse_rgb(&color) {
            let _ = diagram::contrast::luminance(rgb);
            let _ = diagram::contrast::contrast(rgb, (255, 255, 255));
        }
    }
}
