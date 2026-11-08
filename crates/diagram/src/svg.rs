//! The pure half of `diagram.mermaid` ([`crate`]): Ultron theme colours for Pele's CSS variables, the SVG
//! with every `var()` (and `color-mix()`) replaced by a concrete colour, and the PNG rasterization (resvg).
//!
//! Pele colours its SVG with CSS custom properties: the root element's style defines `--_bg:var(--pele-bg,#fff)` and
//! friends, and shapes use `fill="var(--_s)"`. Renderers without CSS variables (resvg, rsvg, most image viewers)
//! draw such shapes black, so the host resolves them itself: a `--pele-*` name takes the theme's colour, a `--_*`
//! name its definition, anything else its fallback.

use resvg::tiny_skia;
use resvg::usvg;
use resvg::usvg::fontdb;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use theme::{Bg, Fg};

/// The colours a diagram is drawn in: `--pele-<name>` values, and whether the theme is dark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagramPalette {
    /// The Ultron theme the colours came from.
    pub theme: String,
    pub dark: bool,
    /// (`--pele-bg`, `#18181e`), ... in a fixed order.
    pub vars: Vec<(String, String)>,
}

impl DiagramPalette {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The page colour the PNG is filled with.
    pub fn background(&self) -> &str {
        self.get("--pele-bg").unwrap_or(defaults(self.dark).page)
    }

    /// What an unresolvable `var()` without a fallback becomes.
    pub fn foreground(&self) -> &str {
        self.get("--pele-fg").unwrap_or(defaults(self.dark).text)
    }
}

fn defaults(dark: bool) -> theme::DefaultColors {
    if dark {
        theme::DARK_DEFAULTS
    } else {
        theme::LIGHT_DEFAULTS
    }
}

/// Theme tokens of the eight series colours (pie slices, chart lines, git branches), in order.
pub const SERIES_TOKENS: [Fg; 8] = [
    Fg::Accent,
    Fg::Success,
    Fg::Warning,
    Fg::Error,
    Fg::Border,
    Fg::ThinkingHigh,
    Fg::MdHeading,
    Fg::SyntaxType,
];

/// The Pele colours of an Ultron theme: page and card from the theme's `export` colours (else its tool background),
/// text, muted text, lines, borders, accent and the series from its colour tokens.
pub fn palette_from_theme(json: &theme::ThemeJson, name: &str) -> Result<DiagramPalette, String> {
    let colors: HashMap<String, String> = theme::resolved_css_colors(json, name)?.into_iter().collect();
    let color = |token: &str| colors.get(token).cloned().unwrap_or_default();
    let text = color(Fg::Text.name());
    let dark = theme::hex_to_rgb(&text)
        .map(|(r, g, b)| theme::theme_for_rgb(r, g, b) == theme::TerminalTheme::Light)
        .unwrap_or(!theme::is_light_theme(name));
    let export = theme::theme_export_colors(json);
    let page = export.page_bg.unwrap_or_else(|| defaults(dark).page.to_string());
    let mut vars = vec![
        ("--pele-bg".to_string(), page),
        ("--pele-fg".to_string(), text.clone()),
        ("--pele-muted".to_string(), color(Fg::Muted.name())),
        ("--pele-line".to_string(), text),
        ("--pele-surface".to_string(), color(Bg::ToolPendingBg.name())),
        ("--pele-surface-alt".to_string(), color(Bg::SelectedBg.name())),
        ("--pele-border".to_string(), color(Fg::Border.name())),
        ("--pele-accent".to_string(), color(Fg::Accent.name())),
        ("--pele-critical".to_string(), color(Fg::Error.name())),
        ("--pele-error".to_string(), color(Fg::Error.name())),
    ];
    for (i, token) in SERIES_TOKENS.iter().enumerate() {
        vars.push((format!("--pele-series-{}", i + 1), color(token.name())));
    }
    vars.retain(|(_, v)| !v.is_empty());
    Ok(DiagramPalette {
        theme: name.to_string(),
        dark,
        vars,
    })
}

/// Which theme a diagram uses: `"dark"` / `"light"` name the built-ins; `"auto"` follows the `theme` setting as the
/// TUI does ([`theme::resolve_theme`]): a `"light/dark"` pair picks the side the terminal's `COLORFGBG` names (as
/// the TUI does without an OSC 11 answer), a custom theme comes from `custom_dir`, no setting takes the built-in of
/// the terminal's side, and a theme that does not load falls back to dark.
pub fn select_palette(
    requested: &str,
    setting: Option<&str>,
    colorfgbg: Option<&str>,
    custom_dir: &std::path::Path,
) -> Result<DiagramPalette, String> {
    select_palette_for_terminal(requested, setting, None, colorfgbg, custom_dir)
}

/// [`select_palette`] with the terminal's side known: `terminal` (`"light"` / `"dark"`, what the attached TUI
/// detected through OSC 11) wins over `COLORFGBG`, which wins over dark. An explicit `requested` theme or a named
/// setting still wins over both.
pub fn select_palette_for_terminal(
    requested: &str,
    setting: Option<&str>,
    terminal: Option<&str>,
    colorfgbg: Option<&str>,
    custom_dir: &std::path::Path,
) -> Result<DiagramPalette, String> {
    match requested {
        "dark" | "light" => {
            let json = theme::builtin_theme_json(requested).expect("dark and light are built in");
            palette_from_theme(&json, requested)
        }
        "auto" => {
            let terminal = match terminal.map(str::trim) {
                Some("light") => theme::TerminalTheme::Light,
                Some("dark") => theme::TerminalTheme::Dark,
                _ => theme::detect_terminal_background_from_env(colorfgbg).theme,
            };
            let (name, json) = theme::resolve_theme(setting, terminal, custom_dir);
            palette_from_theme(&json, &name)
        }
        other => Err(format!(
            "diagram.mermaid theme must be \"auto\", \"dark\" or \"light\", not {other:?}"
        )),
    }
}

/// The end of the parenthesized group whose `(` is just before `start`: the index of its `)`.
fn closing_paren(text: &str, start: usize) -> Option<usize> {
    let mut depth = 1usize;
    for (i, c) in text[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits at the first comma outside parentheses.
fn split_top_comma(text: &str) -> (&str, Option<&str>) {
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return (&text[..i], Some(&text[i + 1..])),
            _ => {}
        }
    }
    (text, None)
}

/// What `var()` references resolve against.
pub struct VarScope<'a> {
    /// `--pele-*` colours of the theme: they win over every definition and fallback.
    pub palette: &'a DiagramPalette,
    /// Custom properties the SVG defines (`--_bg:var(--pele-bg,#fff)`), by name.
    pub defs: HashMap<String, String>,
}

impl VarScope<'_> {
    /// `value` with every `var()` replaced, innermost definitions first. A reference that names nothing and has no
    /// fallback (or a cycle) becomes the theme's foreground colour.
    pub fn resolve(&self, value: &str) -> String {
        self.resolve_at(value, &mut Vec::new())
    }

    fn resolve_at(&self, value: &str, active: &mut Vec<String>) -> String {
        let mut out = String::with_capacity(value.len());
        let mut rest = value;
        while let Some(at) = rest.find("var(") {
            out.push_str(&rest[..at]);
            let inner_start = at + 4;
            let Some(end) = closing_paren(rest, inner_start) else {
                // Unbalanced: keep the text as it is.
                out.push_str(&rest[at..]);
                return out;
            };
            let (name, fallback) = split_top_comma(&rest[inner_start..end]);
            out.push_str(&self.lookup(name.trim(), fallback, active));
            rest = &rest[end + 1..];
        }
        out.push_str(rest);
        out
    }

    fn lookup(&self, name: &str, fallback: Option<&str>, active: &mut Vec<String>) -> String {
        if let Some(color) = self.palette.get(name) {
            return color.to_string();
        }
        if !active.iter().any(|a| a == name)
            && let Some(definition) = self.defs.get(name)
        {
            active.push(name.to_string());
            let resolved = self.resolve_at(definition, active);
            active.pop();
            return resolved;
        }
        match fallback {
            Some(fallback) => self.resolve_at(fallback.trim(), active),
            None => self.palette.foreground().to_string(),
        }
    }
}

/// The custom-property declarations (`--name:value`) of every `style="..."` attribute, by name (the first one wins).
pub fn custom_properties(svg: &str) -> HashMap<String, String> {
    let mut defs = HashMap::new();
    for style in style_attributes(svg) {
        for declaration in split_declarations(style) {
            if let Some((name, value)) = declaration.split_once(':') {
                let name = name.trim();
                if name.starts_with("--") {
                    defs.entry(name.to_string()).or_insert_with(|| value.trim().to_string());
                }
            }
        }
    }
    defs
}

/// The values of `style="..."` attributes inside tags.
fn style_attributes(svg: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = svg;
    while let Some(at) = rest.find(" style=\"") {
        let start = at + 8;
        let Some(len) = rest[start..].find('"') else { break };
        found.push(&rest[start..start + len]);
        rest = &rest[start + len..];
    }
    found
}

/// `;`-separated declarations, ignoring `;` inside parentheses.
fn split_declarations(style: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0usize, 0usize);
    for (i, c) in style.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                parts.push(&style[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&style[start..]);
    parts
}

/// `#rgb` / `#rrggbb` as (r, g, b).
fn parse_hex(color: &str) -> Option<(f64, f64, f64)> {
    let hex = color.trim().strip_prefix('#')?;
    let digits: Vec<u8> = hex
        .chars()
        .map(|c| c.to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()?;
    let (r, g, b) = match digits.as_slice() {
        [r, g, b] => (r * 17, g * 17, b * 17),
        [r1, r2, g1, g2, b1, b2] => (r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2),
        _ => return None,
    };
    Some((f64::from(r), f64::from(g), f64::from(b)))
}

/// One `color-mix()` component: the colour and its percentage, if given.
fn mix_component(text: &str) -> Option<((f64, f64, f64), Option<f64>)> {
    let mut color = None;
    let mut percent = None;
    for word in text.split_whitespace() {
        if let Some(p) = word.strip_suffix('%') {
            percent = Some(p.parse::<f64>().ok()?);
        } else {
            color = Some(parse_hex(word)?);
        }
    }
    Some((color?, percent))
}

/// `color-mix(in srgb, A p%, B q%)` of two hex colours, as CSS computes it: a missing percentage is the other's
/// complement (both missing: 50% each), and the weights are normalized to their sum.
pub fn color_mix(arguments: &str) -> Option<String> {
    let (space, rest) = split_top_comma(arguments);
    if space.trim() != "in srgb" {
        return None;
    }
    let (first, second) = split_top_comma(rest?);
    let (a, pa) = mix_component(first)?;
    let (b, pb) = mix_component(second?)?;
    let (pa, pb) = match (pa, pb) {
        (Some(pa), Some(pb)) => (pa, pb),
        (Some(pa), None) => (pa, 100.0 - pa),
        (None, Some(pb)) => (100.0 - pb, pb),
        (None, None) => (50.0, 50.0),
    };
    let total = pa + pb;
    if total <= 0.0 {
        return None;
    }
    let channel = |x: f64, y: f64| ((x * pa + y * pb) / total).round().clamp(0.0, 255.0) as u8;
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        channel(a.0, b.0),
        channel(a.1, b.1),
        channel(a.2, b.2)
    ))
}

/// `text` with every `color-mix(...)` of hex colours replaced by its result (others stay).
fn replace_color_mix(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("color-mix(") {
        out.push_str(&rest[..at]);
        let start = at + "color-mix(".len();
        let Some(end) = closing_paren(rest, start) else {
            out.push_str(&rest[at..]);
            return out;
        };
        match color_mix(&rest[start..end]) {
            Some(color) => out.push_str(&color),
            None => out.push_str(&rest[at..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// A style attribute without its custom-property declarations.
fn without_custom_properties(style: &str) -> String {
    split_declarations(style)
        .into_iter()
        .filter(|d| !d.trim().is_empty() && !d.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join(";")
}

/// The SVG with every `var()` inside its tags replaced by a concrete value (theme colours for `--pele-*`), every
/// `color-mix()` of the results computed, and the custom-property declarations dropped. Text between tags (the
/// diagram's labels) is never touched.
pub fn flatten_svg(svg: &str, palette: &DiagramPalette) -> String {
    let scope = VarScope {
        palette,
        defs: custom_properties(svg),
    };
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let end = tag_end(rest, open).unwrap_or(rest.len());
        let tag = &rest[open..end];
        out.push_str(&flatten_tag(tag, &scope));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// The index just past the `>` closing the tag that starts at `open` (quotes may hold `>`).
fn tag_end(text: &str, open: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in text[open..].char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return Some(open + i + 1),
            _ => {}
        }
    }
    None
}

fn flatten_tag(tag: &str, scope: &VarScope<'_>) -> String {
    if !tag.contains("var(") && !tag.contains("color-mix(") && !tag.contains("--") {
        return tag.to_string();
    }
    let resolved = replace_color_mix(&scope.resolve(tag));
    // Drop the custom-property declarations from style attributes (and a style left empty).
    let mut out = String::with_capacity(resolved.len());
    let mut rest = resolved.as_str();
    while let Some(at) = rest.find(" style=\"") {
        let start = at + 8;
        let Some(len) = rest[start..].find('"') else { break };
        let style = without_custom_properties(&rest[start..start + len]);
        out.push_str(&rest[..at]);
        if !style.is_empty() {
            out.push_str(" style=\"");
            out.push_str(&style);
            out.push('"');
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

/// The first family of `preferences` that `available` has (case-insensitively), else the first available one.
pub fn pick_family(available: &[String], preferences: &[&str]) -> Option<String> {
    preferences
        .iter()
        .find_map(|want| available.iter().find(|have| have.eq_ignore_ascii_case(want)))
        .or_else(|| available.first())
        .cloned()
}

const SANS: [&str; 9] = [
    "DejaVu Sans",
    "Noto Sans",
    "Liberation Sans",
    "Arial",
    "Helvetica",
    "Inter",
    "Roboto",
    "Cantarell",
    "Ubuntu",
];
const MONO: [&str; 7] = [
    "DejaVu Sans Mono",
    "Noto Sans Mono",
    "Liberation Mono",
    "JetBrains Mono",
    "Fira Mono",
    "Ubuntu Mono",
    "Courier New",
];

/// A font database with `fonts` loaded and its generic families (serif, the last fallback of usvg, too) set to
/// fonts it has.
pub fn font_database(fonts: impl FnOnce(&mut fontdb::Database)) -> fontdb::Database {
    let mut db = fontdb::Database::new();
    fonts(&mut db);
    let mut families: Vec<String> = db
        .faces()
        .filter_map(|face| face.families.first().map(|(name, _)| name.clone()))
        .collect();
    families.sort();
    families.dedup();
    if let Some(sans) = pick_family(&families, &SANS) {
        db.set_sans_serif_family(sans.clone());
        db.set_serif_family(sans);
    }
    if let Some(mono) = pick_family(&families, &MONO) {
        db.set_monospace_family(mono);
    }
    db
}

/// The system's fonts, loaded once per process.
pub fn system_fonts() -> Arc<fontdb::Database> {
    static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| Arc::new(font_database(fontdb::Database::load_system_fonts)))
        .clone()
}

/// The longest side of a PNG, in pixels: larger diagrams are scaled down to it.
pub const MAX_PNG_SIDE: u32 = 8192;

/// A rasterized diagram.
#[derive(Debug)]
pub struct Png {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The SVG drawn at `scale` (2.0: twice its size) on `background`, as PNG bytes.
pub fn rasterize(svg: &str, scale: f32, background: &str, fonts: Arc<fontdb::Database>) -> Result<Png, String> {
    let options = usvg::Options {
        fontdb: fonts,
        font_family: "sans-serif".into(),
        ..usvg::Options::default()
    };
    let tree =
        usvg::Tree::from_str(svg, &options).map_err(|e| format!("diagram.mermaid: the SVG does not parse: {e}"))?;
    let size = tree.size();
    let longest = size.width().max(size.height()) * scale;
    let scale = if longest > MAX_PNG_SIDE as f32 {
        scale * MAX_PNG_SIDE as f32 / longest
    } else {
        scale
    };
    let width = ((size.width() * scale).ceil() as u32).max(1);
    let height = ((size.height() * scale).ceil() as u32).max(1);
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| format!("diagram.mermaid: cannot draw a {width}x{height} image"))?;
    if let Some((r, g, b)) = parse_hex(background) {
        pixmap.fill(tiny_skia::Color::from_rgba8(r as u8, g as u8, b as u8, 255));
    }
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let data = pixmap
        .encode_png()
        .map_err(|e| format!("diagram.mermaid: PNG encoding failed: {e}"))?;
    Ok(Png { data, width, height })
}
