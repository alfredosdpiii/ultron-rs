//! The terminal settings the TUIs apply before they paint (interactive-mode.ts and startup-ui.ts call
//! `setCapabilityOverrides(settingsManager.getTerminalCapabilityOverrides())`): the terminal's capabilities with
//! `terminal.trueColor`, `terminal.hyperlinks` and `terminal.images` forced over the detected ones, and the
//! keybinding platform of the run's environment.

use config::settings::SettingsManager;
use tui::{Capabilities, CapabilityOverrides, ImageProtocol};

use crate::Env;

/// The settings' overrides in the TUI's terms.
pub fn overrides(settings: &SettingsManager) -> CapabilityOverrides {
    let o = config::keys::terminal_capability_overrides(settings.settings());
    CapabilityOverrides {
        images: o.images.map(|protocol| match protocol {
            Some("kitty") => Some(ImageProtocol::Kitty),
            Some("iterm2") => Some(ImageProtocol::Iterm2),
            _ => None,
        }),
        true_color: o.true_color,
        hyperlinks: o.hyperlinks,
    }
}

/// `getCapabilities()` for this run: its environment, the tmux probe and the settings' overrides.
pub fn capabilities(env: &Env, settings: &SettingsManager) -> Capabilities {
    let var = |k: &str| (env.var)(k);
    tui::probed_capabilities(&var, &overrides(settings), &|| {
        tui::tmux_forwards_hyperlinks(&var, tui::TMUX_PROBE_TIMEOUT)
    })
}

/// `useWindowsKeybindings()` on Linux: WSL, named by `WSL_DISTRO_NAME` or `WSL_INTEROP` (core/keybindings.ts; the
/// clipboard's `isWSL` is another rule, tui `is_wsl`).
pub fn wsl_keybindings(env: &Env) -> bool {
    let set = |name: &str| (env.var)(name).is_some_and(|v| !v.is_empty());
    set("WSL_DISTRO_NAME") || set("WSL_INTEROP")
}

/// The keybindings of `ultron setup` and `ultron config`: the app's defaults on this run's platform.
pub fn screen_keys(env: &Env) -> tui::KeybindingsManager {
    tui::KeybindingsManager::new(
        interactive::keybindings::native_app_keybindings(platform(env)),
        Vec::new(),
        Default::default(),
    )
}

/// The keybinding platform of this run.
pub fn platform(env: &Env) -> interactive::keybindings::Platform {
    interactive::keybindings::Platform::new("linux", wsl_keybindings(env))
}
