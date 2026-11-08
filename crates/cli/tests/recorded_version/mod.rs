//! The product version the TypeScript fixtures were recorded at, and the one substitution that lets their recorded
//! output be compared with this build's. Every recording under crates/cli/fixtures (and
//! crates/claude-root/fixtures/claude.json) was made at one version: the `version` cli.json records (`VERSION` of
//! config.ts then). Where a recording prints that version, this build prints `config::version()`; nothing else in it
//! changes.

use std::sync::LazyLock;

/// The version the fixtures were recorded at: cli.json's `version`.
pub fn recorded() -> &'static str {
    static RECORDED: LazyLock<String> = LazyLock::new(|| {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/cli.json")).expect("cli.json parses");
        fixture["version"]
            .as_str()
            .expect("cli.json records the version")
            .to_string()
    });
    &RECORDED
}

/// `text`, recorded at [`recorded`], as this build prints it: a line that is the version alone (`--version`) and each
/// `v<version>` (the TUI header's `ultron v…`, `update`'s `(v…)`) name `config::version()`. Other mentions of the
/// recorded version (a changelog heading `[0.87.39]`) are left as recorded.
pub fn current(text: &str) -> String {
    let (recorded, current) = (recorded(), config::version());
    let tagged = (format!("v{recorded}"), format!("v{current}"));
    text.split_inclusive('\n')
        .map(|line| match line.strip_suffix('\n').unwrap_or(line) == recorded {
            true => line.replacen(recorded, current, 1),
            false => line.replace(&tagged.0, &tagged.1),
        })
        .collect()
}

/// `value` with the string at JSON `pointer` (an MCP `/result/serverInfo/version`, a registry's latest
/// `/body/version`) set to `config::version()` when it is the recorded version.
pub fn current_at(value: &mut serde_json::Value, pointer: &str) {
    if let Some(field) = value.pointer_mut(pointer)
        && field.as_str() == Some(recorded())
    {
        *field = config::version().into();
    }
}
