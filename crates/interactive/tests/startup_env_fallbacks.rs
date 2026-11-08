//! `runClientTui`'s hardware cursor and clear-on-shrink (client-tui.ts): `settingsManager.getShowHardwareCursor()`
//! and `getClearOnShrink()`, whose unset settings fall back to `PI_HARDWARE_CURSOR=1` / `PI_CLEAR_ON_SHRINK=1`
//! (settings-manager.ts; the getters' TypeScript values are recorded in crates/config/fixtures/worker-settings.json).
//! Literal expectations through `App::apply_startup_settings`, which the interactive command calls at startup.

use crate::support;

use interactive::app::{App, AppOptions};
use serde_json::{Value, json};

fn app() -> App {
    App::new(
        AppOptions {
            theme: support::dark_theme(),
            ..support::options()
        },
        std::sync::mpsc::channel().0,
    )
}

fn applied(settings: Value, env: &[(&str, &str)]) -> (bool, bool) {
    let mut app = app();
    let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    app.apply_startup_settings(&settings, &|key| {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    });
    (app.show_hardware_cursor, app.clear_on_shrink)
}

#[test]
fn interactive_startup_hardware_cursor_and_clear_on_shrink_fall_back_to_the_pi_env_vars() {
    let on = [
        (config::keys::HARDWARE_CURSOR_ENV, "1"),
        (config::keys::CLEAR_ON_SHRINK_ENV, "1"),
    ];
    assert_eq!(applied(json!({}), &[]), (false, false));
    assert_eq!(applied(json!({}), &on), (true, true));
    // Only "1" turns them on.
    assert_eq!(
        applied(
            json!({}),
            &[
                (config::keys::HARDWARE_CURSOR_ENV, "true"),
                (config::keys::CLEAR_ON_SHRINK_ENV, "yes")
            ]
        ),
        (false, false)
    );
    assert_eq!(
        applied(json!({}), &[(config::keys::HARDWARE_CURSOR_ENV, "1")]),
        (true, false)
    );
    assert_eq!(
        applied(json!({}), &[(config::keys::CLEAR_ON_SHRINK_ENV, "1")]),
        (false, true)
    );
    // A setting wins over the environment, either way.
    let off = json!({"showHardwareCursor": false, "terminal": {"clearOnShrink": false}});
    assert_eq!(applied(off, &on), (false, false));
    let set = json!({"showHardwareCursor": true, "terminal": {"clearOnShrink": true}});
    assert_eq!(applied(set, &[]), (true, true));
}
