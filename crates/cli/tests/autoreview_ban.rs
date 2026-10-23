//! `ultron autoreview ban | unban | bans` (Rust only) and how `review`, `status` and `once` honour `autoreview.ban`,
//! through `autoreview_cli::run_command_with` against the in-memory fake GitHub of crates/review/tests/github_common
//! (FakeHub behind the injected runner, a scripted engine). A private agent dir; no network, no real gh, no model.

#[path = "../../review/tests/github_common/mod.rs"]
mod github_common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cli::autoreview_cli::{Overrides, run_command_with};
use github_common::{BOT, FakeEngine, FakeHub, FakePull, Notification};
use serde_json::{Value, json};

fn env_for(dir: &Path) -> cli::Env {
    let agent = dir.join("agent");
    let cache = dir.join("cache");
    let home = dir.to_path_buf();
    cli::Env {
        cwd: dir.join("project"),
        home: home.clone(),
        var: Arc::new(move |key| match key {
            "ULTRON_CODING_AGENT_DIR" => Some(agent.display().to_string()),
            "ULTRON_AUTOREVIEW_CACHE_DIR" => Some(cache.display().to_string()),
            "ULTRON_HINDSIGHT_URL" => Some("off".into()),
            "HOME" => Some(home.display().to_string()),
            _ => None,
        }),
        docs: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs"),
    }
}

fn run(env: &cli::Env, overrides: &Overrides, args: &[&str]) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_command_with(&args, env, overrides, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

fn settings(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("agent/settings.json")).unwrap()).unwrap()
}

fn seed(dir: &Path, settings: Value) {
    std::fs::create_dir_all(dir.join("agent")).unwrap();
    std::fs::create_dir_all(dir.join("project")).unwrap();
    std::fs::write(dir.join("agent/settings.json"), settings.to_string()).unwrap();
}

fn hub_with_pull(author: &str) -> Arc<FakeHub> {
    let hub = FakeHub::new();
    hub.add_account(BOT, "github.com", true);
    let mut pull = FakePull::new("o", "r", 1).requested(&[BOT]);
    pull.author = author.into();
    hub.add_pull(pull);
    hub
}

#[test]
fn cli_autoreview_ban_unban_and_bans_write_the_global_settings() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_for(dir.path());
    seed(dir.path(), json!({"theme": "dark", "autoreview": {"model": "p/m"}}));
    let none = Overrides::default();

    assert_eq!(
        run(&env, &none, &["bans"]),
        (0, "Bans (autoreview.ban):\n  none\n".into(), String::new())
    );
    assert_eq!(
        run(&env, &none, &["ban", "lepmoran-boostkh", "--account", "bryandlp-boost"]),
        (
            0,
            "Banned lepmoran-boostkh for bryandlp-boost: autoreview does not review their pull requests there, nor act on their review requests and mentions\n".into(),
            String::new()
        )
    );
    assert_eq!(
        run(&env, &none, &["ban", "@Spammer"]),
        (
            0,
            "Banned Spammer for every account (*): autoreview does not review their pull requests there, nor act on their review requests and mentions\n".into(),
            String::new()
        )
    );
    assert_eq!(
        run(&env, &none, &["ban", "LEPMORAN-BOOSTKH", "--account", "BryanDLP-Boost"]),
        (
            0,
            "LEPMORAN-BOOSTKH is already banned for BryanDLP-Boost\n".into(),
            String::new()
        )
    );
    assert_eq!(
        run(&env, &none, &["ban", "intern", "--account", "ghe.example.com/work"]).0,
        0
    );
    assert_eq!(
        settings(dir.path()),
        json!({"theme": "dark", "autoreview": {"model": "p/m", "ban": {
            "bryandlp-boost": ["lepmoran-boostkh"], "*": ["Spammer"], "ghe.example.com/work": ["intern"]
        }}})
    );
    assert_eq!(
        run(&env, &none, &["bans"]),
        (
            0,
            "Bans (autoreview.ban):\n  bryandlp-boost: lepmoran-boostkh\n  every account (*): Spammer\n  ghe.example.com/work: intern\n".into(),
            String::new()
        )
    );
    let (code, out, _) = run(&env, &none, &["bans", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap(),
        json!({"bryandlp-boost": ["lepmoran-boostkh"], "*": ["Spammer"], "ghe.example.com/work": ["intern"]})
    );

    // unban without --account lifts the ban for every account (*) only, and says where else the login is banned.
    assert_eq!(
        run(&env, &none, &["unban", "lepmoran-boostkh"]),
        (
            1,
            String::new(),
            "Error: lepmoran-boostkh is not banned for every account (*); it is banned for bryandlp-boost (--account bryandlp-boost)\n".into()
        )
    );
    assert_eq!(
        run(
            &env,
            &none,
            &["unban", "lepmoran-boostkh", "--account", "bryandlp-boost"]
        ),
        (
            0,
            "Unbanned lepmoran-boostkh for bryandlp-boost\n".into(),
            String::new()
        )
    );
    assert_eq!(
        run(&env, &none, &["unban", "spammer"]),
        (0, "Unbanned spammer for every account (*)\n".into(), String::new())
    );
    run(&env, &none, &["unban", "intern", "--account", "GHE.example.com/work"]);
    assert_eq!(
        settings(dir.path()),
        json!({"theme": "dark", "autoreview": {"model": "p/m"}})
    );

    // Usage mistakes exit 2 and change nothing.
    for (args, error) in [
        (&["ban"][..], "Error: ban needs a GitHub login\n"),
        (
            &["ban", "not/a/login"][..],
            "Error: not a GitHub login: \"not/a/login\"\n",
        ),
        (
            &["ban", "x", "--account", "two words"][..],
            "Error: --account takes a login, host/login or *, not \"two words\"\n",
        ),
        (&["ban", "a", "b"][..], "Error: unexpected argument: b\n"),
    ] {
        assert_eq!(
            run(&env, &none, args),
            (2, String::new(), error.to_string()),
            "{args:?}"
        );
    }
    assert_eq!(
        settings(dir.path()),
        json!({"theme": "dark", "autoreview": {"model": "p/m"}})
    );
}

/// `ultron autoreview review <pr>` of a banned author's pull request is refused, naming the ban: nothing is read
/// past the pull request, nothing is posted, the engine is not called.
#[test]
fn cli_autoreview_review_of_a_banned_author_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_for(dir.path());
    seed(
        dir.path(),
        json!({"autoreview": {"ban": {"ultron-bot": ["lepmoran-boostkh"]}}}),
    );
    let hub = hub_with_pull("Lepmoran-BoostKH");
    let engine = FakeEngine::new(Vec::new());
    let overrides = Overrides {
        runner: Some(hub.clone()),
        engine: Some(engine.clone()),
    };
    let (code, out, err) = run(&env, &overrides, &["review", "o/r#1"]);
    assert_eq!((code, out.as_str()), (1, ""));
    assert_eq!(
        err,
        "Error: github.com/o/r#1 as ultron-bot: not reviewed: its author Lepmoran-BoostKH is banned for ultron-bot (autoreview.ban; \"ultron autoreview unban Lepmoran-BoostKH --account ultron-bot\" lifts it)\n"
    );
    assert!(hub.api("^POST ").is_empty());
    assert!(engine.specs().is_empty());
}

/// `status` lists the bans (text and `--json`); `once` skips a banned author's pull request, read from the settings
/// file the daemon re-reads.
#[test]
fn cli_autoreview_status_shows_the_bans_and_once_skips_a_banned_author() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_for(dir.path());
    seed(dir.path(), json!({"autoreview": {"ban": {"*": ["lepmoran-boostkh"]}}}));
    let hub = hub_with_pull("lepmoran-boostkh");
    hub.lock().notifications = vec![Notification::new("review_requested", 1)];
    let engine = FakeEngine::new(Vec::new());
    let overrides = Overrides {
        runner: Some(hub.clone()),
        engine: Some(engine.clone()),
    };
    let (code, out, _) = run(&env, &overrides, &["status"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("\n\nBans (autoreview.ban):\n  every account (*): lepmoran-boostkh\n\nQueue: empty\n"),
        "{out}"
    );
    let (_, out, _) = run(&env, &overrides, &["status", "--json"]);
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["ban"],
        json!({"*": ["lepmoran-boostkh"]})
    );

    let (code, out, err) = run(&env, &overrides, &["once", "--json"]);
    assert_eq!(code, 0, "{err}");
    let reviews = serde_json::from_str::<Value>(&out).unwrap()["reviews"].clone();
    assert_eq!(reviews[0]["kind"], "skipped");
    assert_eq!(reviews[0]["reason"], "author banned: lepmoran-boostkh");
    // The daemon's log (its file; its stderr is the process's own).
    let logs = dir.path().join("agent/autoreview/logs");
    let logged: String = std::fs::read_dir(&logs)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert_eq!(
        logged
            .lines()
            .filter(|line| line.ends_with(" github.com/o/r#1 as ultron-bot: skipped (author banned: lepmoran-boostkh)"))
            .count(),
        1,
        "{logged}"
    );
    assert!(hub.api("^POST ").is_empty());
    assert!(engine.specs().is_empty());
}
