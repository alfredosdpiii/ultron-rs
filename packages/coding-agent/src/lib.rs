//! The package files the `ultron` binary embeds: the Python runtime and its modules, bundled skills, docs, the
//! changelog, the HTML export template and the Loki engine. The files live in this crate's directory so they ship in
//! its tarball (crates.io drops files outside a crate). `build.rs` writes the file table `FILES`, `HASH` and
//! `CHECKOUT`; the constants below name single files for the crates that read them.

/// One embedded file: its path inside the package directory, its mode and its bytes.
pub struct Asset {
    pub path: &'static str,
    pub mode: u32,
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// `package.json`: the product version.
pub const PACKAGE_JSON: &str = include_str!("../package.json");
/// The secret patterns the output masker and the runtime share.
pub const SECRET_PATTERNS: &str = include_str!("ultron/rlm/secret-patterns.json");
/// The mascot the easter eggs show.
pub const CLANKOLAS_PNG: &[u8] = include_bytes!("modes/interactive/assets/clankolas.png");
pub const EXPORT_HTML_TEMPLATE: &str = include_str!("core/export-html/template.html");
pub const EXPORT_HTML_CSS: &str = include_str!("core/export-html/template.css");
pub const EXPORT_HTML_JS: &str = include_str!("core/export-html/template.js");
pub const MARKED_JS: &str = include_str!("core/export-html/vendor/marked.min.js");
pub const HIGHLIGHT_JS: &str = include_str!("core/export-html/vendor/highlight.min.js");
pub const LOKI_ENGINE: &[u8] = include_bytes!("ultron/loki-engine/loki.py");
pub const LOKI_TEMPLATE: &[u8] = include_bytes!("ultron/loki-engine/templates/loki.json");
/// The engine pin (`VERSION.json`): the commit the bundled engine was taken from.
pub const LOKI_VERSION: &str = include_str!("ultron/loki-engine/VERSION.json");
