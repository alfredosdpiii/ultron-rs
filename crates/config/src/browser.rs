//! `openBrowser` (utils/open-browser.ts) and `remote-browser.ts`: the platform's launcher, found only on the PATH the
//! session was started with and run detached with only that PATH, HOME and the desktop variables; and where this
//! process runs relative to the user's browser, for the login hint.

use std::sync::Arc;

/// `PI_OAUTH_CALLBACK_HOST`, the older name of [`OAUTH_CALLBACK_HOST_ENV`].
pub const LEGACY_OAUTH_CALLBACK_HOST_ENV: &str = "PI_OAUTH_CALLBACK_HOST";
/// Where a login's OAuth callback server listens instead of loopback; set, the browser is taken to reach it.
pub const OAUTH_CALLBACK_HOST_ENV: &str = "ULTRON_OAUTH_CALLBACK_HOST";

/// `openBrowser`'s launcher for `platform` (Node's `process.platform`): `open` on macOS, `rundll32
/// url.dll,FileProtocolHandler` on Windows (never `cmd /c start`, which would re-parse the URL), else `xdg-open`.
pub fn browser_command(platform: &str, target: &str) -> (String, Vec<String>) {
    match platform {
        "darwin" => ("open".into(), vec![target.into()]),
        "win32" => (
            "rundll32".into(),
            vec!["url.dll,FileProtocolHandler".into(), target.into()],
        ),
        _ => ("xdg-open".into(), vec![target.into()]),
    }
}

pub type UrlOpener = Arc<dyn Fn(&str) + Send + Sync>;

/// The session variables a browser launcher needs to reach the desktop (xdg-open finds the display and the
/// session bus through them); only these pass to it besides PATH and HOME.
const OPENER_VARS: [&str; 8] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XAUTHORITY",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_TYPE",
    "BROWSER",
];

pub fn opener_vars(var: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    OPENER_VARS
        .iter()
        .filter_map(|name| var(name).map(|value| (name.to_string(), value)))
        .collect()
}

/// `openBrowser` with the launcher found on `path` (the session's PATH, never the process's; `None` finds none), run
/// detached (its own process group, no stdio, reaped in the background) with only that PATH, `home` and `desktop`
/// (from [`opener_vars`]) in its environment. A missing launcher opens nothing and a failing one is ignored.
pub fn browser_opener(path: Option<String>, home: Option<String>, desktop: Vec<(String, String)>) -> UrlOpener {
    Arc::new(move |url: &str| {
        let (name, args) = browser_command(jsrt::process::platform(), url);
        let Some(launcher) = path.as_deref().and_then(|path| jsrt::process::which(&name, path)) else {
            return;
        };
        let mut command = std::process::Command::new(launcher);
        command
            .args(&args)
            .env_clear()
            .env("PATH", path.as_deref().unwrap_or_default())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(home) = &home {
            command.env("HOME", home);
        }
        command.envs(desktop.iter().map(|(name, value)| (name, value)));
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        if let Ok(mut child) = command.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    })
}

/// Where this process runs relative to the user's browser (`browserReach`).
pub fn browser_reach(var: &dyn Fn(&str) -> Option<String>, exists: &dyn Fn(&str) -> bool) -> &'static str {
    let set = |key: &str| var(key).is_some_and(|v| !v.trim().is_empty());
    if set(OAUTH_CALLBACK_HOST_ENV) || set(LEGACY_OAUTH_CALLBACK_HOST_ENV) {
        return "local";
    }
    let container = set("container")
        || set("KUBERNETES_SERVICE_HOST")
        || exists("/.dockerenv")
        || exists("/run/.containerenv")
        || std::fs::read_to_string("/proc/1/cgroup").is_ok_and(|cgroup| {
            ["docker", "containerd", "kubepods", "libpod", "lxc"]
                .iter()
                .any(|marker| cgroup.contains(marker))
        });
    if container {
        return "container";
    }
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        "ssh"
    } else {
        "local"
    }
}

const UNREACHABLE_CALLBACK_STEP: &str = "copy that page's full address from the address bar and paste it here.";

/// `unreachableCallbackHint`: what to do when the browser cannot load the login's localhost callback page.
pub fn unreachable_callback_hint(reach: &str) -> String {
    if reach == "local" {
        return format!(
            "If the browser ends on a page that can't be reached (localhost refused to connect), {UNREACHABLE_CALLBACK_STEP}"
        );
    }
    let place = if reach == "container" {
        "in a container"
    } else {
        "over SSH"
    };
    format!(
        "Ultron is running {place}, so the browser on your computer may not reach this login's callback. If it ends on a page that can't be reached (localhost refused to connect) after you sign in, that is expected: {UNREACHABLE_CALLBACK_STEP}"
    )
}
