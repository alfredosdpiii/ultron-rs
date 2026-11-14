//! The language servers Ultron knows: which files each checks, where its project root is, and how mise installs it.
//! Every row was run against the real server (`cargo run -p lsp --example check -- <file>`).

use crate::client::Diagnostic;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub struct Spec {
    pub name: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// `mise exec <these> -- <command> <args>` runs the server when it is not on PATH, installing it first.
    pub mise: &'static [&'static str],
    /// File extension and the LSP `languageId` of such a file.
    pub languages: &'static [(&'static str, &'static str)],
    /// Files that mark a project root.
    pub markers: &'static [&'static str],
    /// True when the server is useful without a marker (the repository root is its root then); false when it
    /// reports false errors without its project file.
    pub loose: bool,
    /// The server says when it has loaded the project (`experimental/serverStatus`); asked before that, it answers
    /// with no diagnostics.
    pub status: bool,
}

const TYPESCRIPT: &[(&str, &str)] = &[
    ("ts", "typescript"),
    ("mts", "typescript"),
    ("cts", "typescript"),
    ("tsx", "typescriptreact"),
    ("js", "javascript"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("jsx", "javascriptreact"),
];

pub const SERVERS: &[Spec] = &[
    Spec {
        name: "rust-analyzer",
        command: "rust-analyzer",
        args: &[],
        mise: &["rust-analyzer"],
        languages: &[("rs", "rust")],
        markers: &["Cargo.toml"],
        loose: false,
        status: true,
    },
    Spec {
        name: "pyright",
        command: "pyright-langserver",
        args: &["--stdio"],
        mise: &["npm:pyright"],
        languages: &[("py", "python"), ("pyi", "python")],
        markers: &["pyproject.toml", "pyrightconfig.json", "setup.py", "requirements.txt"],
        loose: true,
        status: false,
    },
    Spec {
        name: "typescript-language-server",
        command: "typescript-language-server",
        args: &["--stdio"],
        mise: &["npm:typescript-language-server", "npm:typescript"],
        languages: TYPESCRIPT,
        markers: &["tsconfig.json", "jsconfig.json", "package.json"],
        loose: true,
        status: false,
    },
    Spec {
        name: "gopls",
        command: "gopls",
        args: &["serve"],
        mise: &["go:golang.org/x/tools/gopls"],
        languages: &[("go", "go")],
        markers: &["go.work", "go.mod"],
        loose: false,
        status: false,
    },
    Spec {
        name: "clangd",
        command: "clangd",
        args: &[],
        mise: &["github:clangd/clangd"],
        languages: &[
            ("c", "c"),
            ("h", "c"),
            ("cc", "cpp"),
            ("cpp", "cpp"),
            ("cxx", "cpp"),
            ("hh", "cpp"),
            ("hpp", "cpp"),
            ("hxx", "cpp"),
        ],
        // Without the compile flags every project include is an error.
        markers: &["compile_commands.json", "compile_flags.txt", ".clangd"],
        loose: false,
        status: false,
    },
    Spec {
        name: "zls",
        command: "zls",
        args: &[],
        mise: &["zls"],
        languages: &[("zig", "zig")],
        markers: &["build.zig"],
        loose: true,
        status: false,
    },
    Spec {
        name: "lua-language-server",
        command: "lua-language-server",
        args: &[],
        mise: &["lua-language-server"],
        languages: &[("lua", "lua")],
        markers: &[".luarc.json", ".luarc.jsonc"],
        loose: true,
        status: false,
    },
    Spec {
        name: "terraform-ls",
        command: "terraform-ls",
        args: &["serve"],
        mise: &["terraform-ls"],
        languages: &[("tf", "terraform"), ("tfvars", "terraform-vars")],
        markers: &[".terraform.lock.hcl"],
        loose: true,
        status: false,
    },
    Spec {
        name: "bash-language-server",
        command: "bash-language-server",
        args: &["start"],
        mise: &["npm:bash-language-server"],
        languages: &[("sh", "shellscript"), ("bash", "shellscript")],
        markers: &[],
        loose: true,
        status: false,
    },
    Spec {
        name: "yaml-language-server",
        command: "yaml-language-server",
        args: &["--stdio"],
        mise: &["npm:yaml-language-server"],
        languages: &[("yaml", "yaml"), ("yml", "yaml")],
        markers: &[],
        loose: true,
        status: false,
    },
    Spec {
        name: "taplo",
        command: "taplo",
        args: &["lsp", "stdio"],
        mise: &["taplo"],
        languages: &[("toml", "toml")],
        markers: &[],
        loose: true,
        status: false,
    },
];

/// `errors` without those the server only guesses at. rust-analyzer runs without build scripts and proc macros
/// (see `settings`), so what a derive or an `include!` of generated code would define is unknown to it: on 130
/// files of this repository that compile, its experimental diagnostics reported 239 errors of these kinds.
pub fn reliable(spec: &Spec, errors: Vec<Diagnostic>) -> Vec<Diagnostic> {
    const GUESSED: &[&str] = &["E0277", "E0282", "E0432", "E0599"];
    const UNEXPANDED: &[&str] = &["macro-error", "unresolved-proc-macro", "unresolved-macro-call"];
    if spec.name != "rust-analyzer" {
        return errors;
    }
    let code = |error: &Diagnostic| error.code.clone().unwrap_or_default();
    // The names a macro that was not expanded would define are not missing.
    let unexpanded = errors.iter().any(|error| UNEXPANDED.contains(&code(error).as_str()));
    errors
        .into_iter()
        .filter(|error| {
            let code = code(error);
            !(GUESSED.contains(&code.as_str())
                || UNEXPANDED.contains(&code.as_str())
                || error.message.starts_with("Syntax Error in Expansion")
                || unexpanded && code == "E0425")
        })
        .collect()
}

/// TypeScript 7 with the Effect diagnostics built in. TypeScript 7 loads no plugins, so `@effect/language-service`
/// only works in the tsserver of TypeScript 5 and 6.
static EFFECT_TSGO: Spec = Spec {
    name: "effect-tsgo",
    command: "effect-tsgo",
    args: &["tsc", "--lsp", "--stdio"],
    mise: &["npm:@effect/tsgo"],
    languages: TYPESCRIPT,
    markers: &["tsconfig.json", "jsconfig.json", "package.json"],
    loose: true,
    status: false,
};

/// The server for `root` where `spec` is the server of the file's extension: a project whose tsconfig.json names
/// the Effect plugin gets `effect-tsgo`, unless its own TypeScript is 5 or 6, where tsserver loads the plugin.
/// ponytail: only the root tsconfig.json is read (where the plugin's README puts it for a monorepo), and mise's
/// `@effect/tsgo` runs, not the version the project pins; prefer `node_modules/.bin/effect-tsgo` if they drift.
pub fn in_project(spec: &'static Spec, root: &Path) -> &'static Spec {
    if spec.name != "typescript-language-server" {
        return spec;
    }
    let read = |file: &str| std::fs::read_to_string(root.join(file)).ok();
    // tsconfig.json has comments, so it is not parsed.
    let effect = read("tsconfig.json").is_some_and(|text| text.contains("\"@effect/language-service\""));
    let older = read("node_modules/typescript/package.json")
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|package| package["version"].as_str()?.split('.').next()?.parse::<u32>().ok())
        .is_some_and(|major| major < 7);
    if effect && !older { &EFFECT_TSGO } else { spec }
}

/// The server and `languageId` of `file`, by its extension.
pub fn for_file(file: &Path) -> Option<(&'static Spec, &'static str)> {
    let extension = file.extension()?.to_str()?;
    SERVERS.iter().find_map(|spec| {
        let (_, language) = spec.languages.iter().find(|(known, _)| *known == extension)?;
        Some((spec, *language))
    })
}

/// The project `file` belongs to: inside a Git repository the outermost directory with a marker (a Cargo workspace
/// gets one server, not one per crate), or the repository root for a `loose` server; outside one the nearest
/// directory with a marker. None: no server runs (a scratch file in /tmp is not a project).
pub fn root_of(spec: &Spec, file: &Path) -> Option<PathBuf> {
    let (mut nearest, mut outermost) = (None, None);
    for dir in file.ancestors().skip(1) {
        if spec.markers.iter().any(|marker| dir.join(marker).exists()) {
            nearest = nearest.or(Some(dir));
            outermost = Some(dir);
        }
        if dir.join(".git").exists() {
            return outermost.or(spec.loose.then_some(dir)).map(Path::to_path_buf);
        }
    }
    nearest.map(Path::to_path_buf)
}

/// The settings tree a server reads (`initializationOptions`, `workspace/configuration`), by section.
pub fn settings(spec: &Spec, root: &Path, var: &dyn Fn(&str) -> Option<String>) -> Value {
    match spec.name {
        // No `cargo check`, build scripts or proc-macro builds: rust-analyzer's own analysis only, so it never
        // competes with the session's builds for cores or the target directory.
        "rust-analyzer" => json!({ "rust-analyzer": {
            "checkOnSave": false,
            "cachePriming": { "enable": false },
            "cargo": { "buildScripts": { "enable": false } },
            "procMacro": { "enable": false },
            // Unresolved names, which only the experimental diagnostics report; `reliable` drops the rest of them.
            "diagnostics": { "experimental": { "enable": true } },
        } }),
        // Without the project's interpreter every third-party import is an error.
        "pyright" => match loki::project::find_project_python(root, var) {
            Some(python) => json!({ "python": { "pythonPath": python } }),
            None => json!({}),
        },
        _ => json!({}),
    }
}
