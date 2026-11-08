//! Differential parity of the theme registry (theme list, initTheme / setTheme with dark fallback, loadTheme,
//! setRegisteredThemes) against packages/coding-agent theme.ts.
//! Fixture: fixtures/theme-registry-parity.json (`node crates/theme/fixtures/generate-theme-registry.mjs`). The Rust
//! registry has no change callback and no `setThemeInstance`: the fixture's `changes` counts and its `instance` step
//! are not replayed.

use serde_json::{Value, json};
use theme::registry::ThemeRegistry;
use theme::{ColorMode, load_theme_from_path};

fn accent(theme: Option<&theme::Theme>) -> Value {
    theme.map_or(Value::Null, |t| json!(t.fg(theme::Fg::Accent, "x")))
}

#[test]
fn tui_theme_registry_matches_ts() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/theme-registry-parity.json");
    let f: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json");
    let root = std::env::temp_dir().join(format!("tui-theme-registry-{}", std::process::id()));
    let custom = root.join("themes");
    std::fs::create_dir_all(&custom).expect("mkdir");
    for (file, content) in f["files"].as_object().expect("files") {
        std::fs::write(custom.join(file), content.as_str().expect("content")).expect("write");
    }
    let norm = |s: &str| s.replace(&custom.display().to_string(), "<custom>");
    let mut reg = ThemeRegistry::new(custom.clone(), ColorMode::Truecolor);
    for step in f["steps"].as_array().expect("steps") {
        if step["op"] == "instance" {
            continue;
        }
        let arg = step["arg"].as_str().unwrap_or("");
        let got = match step["op"].as_str().expect("op") {
            "list" => {
                let list: Vec<Value> = reg
                    .available_themes_with_paths()
                    .into_iter()
                    .map(|t| {
                        // TypeScript lists the built-ins' files in its themes directory; here they are embedded.
                        let path = match t.path.as_deref() {
                            Some(path) => Some(norm(path)),
                            None if ["dark", "light"].contains(&t.name.as_str()) => {
                                Some(format!("<builtin>/{}.json", t.name))
                            }
                            None => None,
                        };
                        json!({"name": t.name, "path": path})
                    })
                    .collect();
                json!({"op": "list", "result": list})
            }
            "init" | "set" => {
                let result = match step["op"].as_str().unwrap() {
                    "init" => {
                        reg.init_theme(Some(arg), "dark");
                        Value::Null
                    }
                    _ => match reg.set_theme(arg) {
                        Ok(()) => json!({"success": true, "error": null}),
                        Err(e) => json!({"success": false, "error": norm(&e)}),
                    },
                };
                let current = reg.current();
                json!({"op": step["op"], "arg": arg, "result": result,
                    "current": {"name": current.and_then(|t| t.name.clone()), "accent": accent(current)}})
            }
            "byName" => json!({"op": "byName", "arg": arg, "result": accent(reg.load_theme(arg).ok().as_ref())}),
            "registerInvalid" => {
                let mut theme = load_theme_from_path(&custom.join("Zed.json"), ColorMode::Truecolor).expect("Zed");
                theme.name = Some("a/b".into());
                json!({"op": "registerInvalid", "result": reg.set_registered_themes(vec![theme]).err()})
            }
            other => panic!("unknown op {other}"),
        };
        let mut want = step.clone();
        if let Some(step) = want.as_object_mut() {
            step.remove("changes");
        }
        let mut got = got;
        // serde_json and V8 word JSON syntax errors differently; compare only the prefix of those messages.
        for v in [&mut want, &mut got] {
            if let Some(e) = v.pointer_mut("/result/error")
                && let Some(s) = e.as_str()
                && let Some(i) = s.find("SyntaxError:")
            {
                *e = json!(s[..i].to_string());
            }
        }
        assert_eq!(got, want, "step {step}");
        // The generator registers `reg` right after `setTheme("reg")` first fails.
        if step["op"] == "set" && arg == "reg" && step["result"]["success"] == false {
            let theme = load_theme_from_path(&custom.join("reg-source.json"), ColorMode::Truecolor).expect("reg");
            reg.set_registered_themes(vec![theme]).expect("register");
        }
    }
    std::fs::remove_dir_all(&root).ok();
}

/// The theme list sorts names with `localeCompare` (ICU root): punctuation before digits, `_` before `-`, accents a
/// secondary difference (node 26: `["f", "a1", "é", "a-b", "a_b"].sort((a, b) => a.localeCompare(b))` is
/// `["a_b", "a-b", "a1", "é", "f"]`).
#[test]
fn tui_theme_registry_lists_names_in_locale_compare_order() {
    let root = std::env::temp_dir().join(format!("tui-theme-order-{}", std::process::id()));
    let custom = root.join("themes");
    std::fs::create_dir_all(&custom).expect("mkdir");
    for name in ["f", "a1", "é", "a-b", "a_b"] {
        let json = theme::DARK_JSON.replacen("\"name\": \"dark\"", &format!("\"name\": \"{name}\""), 1);
        std::fs::write(custom.join(format!("{name}.json")), json).expect("write");
    }
    let reg = ThemeRegistry::new(custom, ColorMode::Truecolor);
    let names: Vec<String> = reg
        .available_themes_with_paths()
        .into_iter()
        .map(|t| t.name)
        .filter(|n| n != "dark" && n != "light")
        .collect();
    assert_eq!(names, ["a_b", "a-b", "a1", "é", "f"]);
    std::fs::remove_dir_all(&root).ok();
}
