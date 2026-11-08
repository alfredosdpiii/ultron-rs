//! The batch-2 keybinding ids resolve through `KeybindingsManager` (defaults from omp app-keybindings.ts where the
//! key is free in Ultron) and the Esc-Esc double tap.

use std::time::{Duration, Instant};

use interactive::keybindings::{DoubleTap, native_app_keybindings, qol_keybindings};
use interactive::keybindings::{Platform, app_keybindings};
use tui::KeybindingsManager;

fn manager(user: Vec<(String, Option<Vec<String>>)>) -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        user,
        Default::default(),
    )
}

#[test]
fn tui_qol_keys_extend_the_ts_bindings_without_duplicate_ids() {
    let base = app_keybindings(Platform::new("linux", false));
    let all = native_app_keybindings(Platform::new("linux", false));
    assert_eq!(all.len(), base.len() + qol_keybindings().len());
    assert_eq!(&all[..base.len()], &base[..], "the TS list stays first and unchanged");
    let mut ids: Vec<&str> = all.iter().map(|d| d.id).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), before, "no id is defined twice");
}

#[test]
fn tui_qol_keys_retry_is_f5_and_alt_r_stays_history_search() {
    let k = manager(Vec::new());
    assert!(k.matches("\x1b[15~", "app.retry"), "F5 retries");
    assert!(
        !k.matches("\x1br", "app.retry"),
        "omp's alt+r is Ultron's history search"
    );
    assert!(k.matches("\x1br", "app.history.search"));
    assert!(k.matches("\x1bl", "app.display.reset"));
    assert!(k.matches("\x1ba", "app.agents.hub"));
    // Among global app keys, F5 and Alt+L are claimed only by the new ids.
    for id in ["app.retry", "app.display.reset", "app.agents.hub"] {
        let key = &k.get_keys(id)[0];
        let owners: Vec<&str> = native_app_keybindings(Platform::new("linux", false))
            .iter()
            .filter(|d| !d.id.contains("Hub.") && !d.id.starts_with("app.models.") && !d.id.starts_with("app.tree."))
            .filter(|d| d.default_keys.contains(key))
            .map(|d| d.id)
            .collect();
        assert_eq!(owners, vec![id], "{key}");
    }
}

#[test]
fn tui_qol_keys_overlay_letters_and_user_overrides_resolve() {
    let k = manager(vec![("app.retry".into(), Some(vec!["ctrl+shift+r".into()]))]);
    assert!(
        !k.matches("\x1b[15~", "app.retry"),
        "a user override replaces the default"
    );
    assert_eq!(k.get_keys("app.retry"), vec!["ctrl+shift+r"]);
    assert!(k.matches("G", "app.agentHub.bottom"));
    assert!(!k.matches("g", "app.agentHub.bottom"));
    assert!(k.matches("g", "app.agentHub.top"));
    assert!(k.matches("O", "app.copy.open") && k.matches("o", "app.copy.open"));
    assert!(k.matches("\x1b[1;2A", "app.transcript.scrollUp"));
    assert!(k.matches("\x12", "app.historySearch.older"));
}

#[test]
fn tui_qol_keys_double_tap_fires_once_within_the_window() {
    let t0 = Instant::now();
    let mut tap = DoubleTap::new(Duration::from_millis(500));
    assert!(!tap.tap(t0));
    assert!(tap.tap(t0 + Duration::from_millis(400)));
    assert!(!tap.tap(t0 + Duration::from_millis(450)), "a third tap starts over");
    assert!(!tap.tap(t0 + Duration::from_millis(1200)), "outside the window");
    tap.reset();
    assert!(
        !tap.tap(t0 + Duration::from_millis(1300)),
        "another key broke the sequence"
    );
    assert!(tap.tap(t0 + Duration::from_millis(1800)));
}
