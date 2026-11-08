//! A cell cannot change the profile's settings, keys or skills (session_core::compose wires
//! `config::paths::AgentPaths::protected` into the host's file hooks): `write()` raises and a shell command that
//! names them is refused, while the project's own files are written as before. New in Rust: literal expectations.

use crate::p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool};
use serde_json::{Value, json};

const CELL: &str = r#"import json
out = {}
for path in ("../agent/settings.json", "../agent/skills/x/SKILL.md"):
    try:
        await write(path, "{}")
        out[path] = "written"
    except ValueError as e:
        out[path] = str(e)
try:
    out["bash"] = str(await bash('''echo '{}' > ../agent/auth.json'''))
except RuntimeError as e:
    out["bash"] = str(e)
await write("notes.txt", "fine")
print('out', json.dumps(out))"#;

#[test]
fn session_core_a_cell_cannot_write_the_profiles_settings_keys_or_skills() {
    let a = actor(
        in_order(vec![tool("toolu_1", CELL), text("done")]),
        Setup {
            settings: Some(json!({"quietStartup": true})),
            ..Setup::default()
        },
    );
    let ended = a.prompt("change the settings");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let results = a.requests().last().map(p_session_support::tool_results).unwrap();
    let output = results[0].as_str().unwrap().to_string();
    let line = output.lines().find_map(|l| l.strip_prefix("out ")).expect(&output);
    let out: Value = serde_json::from_str(line).unwrap();
    for key in ["../agent/settings.json", "../agent/skills/x/SKILL.md", "bash"] {
        assert!(
            out[key].as_str().unwrap().contains("is the user's to change"),
            "{key}: {out}"
        );
    }
    let agent = a.root.join("agent");
    assert_eq!(
        std::fs::read_to_string(agent.join("settings.json")).unwrap(),
        json!({"quietStartup": true}).to_string()
    );
    assert!(!agent.join("auth.json").exists() && !agent.join("skills/x").exists());
    assert_eq!(std::fs::read_to_string(a.cwd.join("notes.txt")).unwrap(), "fine");
}
