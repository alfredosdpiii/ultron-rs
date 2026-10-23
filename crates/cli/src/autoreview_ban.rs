//! `ultron autoreview ban | unban | bans` and `/autoreview ban | unban`: `autoreview.ban` in the global settings
//! file, written through the settings manager (its lock, a fresh read of the file, and a merge of only the
//! `autoreview.ban` key). A running daemon reads the change at its next poll.

use review::ban::{
    ALL, BanList, add_ban, describe_key, normalize_login, remove_ban, same_login, valid_account_key, valid_login,
};
use serde_json::{Map, Value};

use crate::Env;

/// Why a ban change was refused: a usage mistake (exit 2) or anything else (exit 1).
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    Usage(String),
    Error(String),
}

fn manager(env: &Env) -> config::settings::SettingsManager {
    config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false)
}

/// The bans the global settings hold now.
pub fn current(env: &Env) -> BanList {
    BanList::from_value(manager(env).autoreview_settings().get("ban"))
}

/// One line per account key: `  <key>: <login>, <login>`; `none` when there is no ban.
pub fn ban_lines(list: &BanList) -> Vec<String> {
    let rules: Vec<String> = list
        .rules()
        .iter()
        .filter(|(_, logins)| !logins.is_empty())
        .map(|(key, logins)| format!("  {}: {}", describe_key(key), logins.join(", ")))
        .collect();
    if rules.is_empty() {
        return vec!["  none".into()];
    }
    rules
}

/// The bans in one clause, for `/autoreview`'s status line.
pub fn summary(list: &BanList) -> String {
    let rules: Vec<String> = list
        .rules()
        .iter()
        .filter(|(_, logins)| !logins.is_empty())
        .map(|(key, logins)| format!("{} for {}", logins.join(", "), describe_key(key)))
        .collect();
    if rules.is_empty() {
        "no bans".into()
    } else {
        format!("banned: {}", rules.join("; "))
    }
}

/// Ban (`add`) or unban `login` for the reviewing account `account` (a login, `host/login` or `*`; none is `*`, every
/// account). The message says what changed.
pub fn change(env: &Env, login: &str, account: Option<&str>, add: bool) -> Result<String, Refused> {
    let login = normalize_login(login);
    if !valid_login(&login) {
        return Err(Refused::Usage(format!("not a GitHub login: {login:?}")));
    }
    let key = account.map(str::trim).unwrap_or(ALL);
    if !valid_account_key(key) {
        return Err(Refused::Usage(format!(
            "--account takes a login, host/login or *, not {key:?}"
        )));
    }
    let mut settings = manager(env);
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return Err(Refused::Error(format!(
            "{} cannot be read ({}); nothing changed",
            error.path.display(),
            error.message
        )));
    }
    let mut ban = match settings
        .global_settings()
        .get("autoreview")
        .and_then(|autoreview| autoreview.get("ban"))
    {
        Some(Value::Object(ban)) => ban.clone(),
        _ => Map::new(),
    };
    let target = describe_key(key);
    let changed = if add {
        add_ban(&mut ban, key, &login)
    } else {
        remove_ban(&mut ban, key, &login)
    };
    if !changed {
        if add {
            return Ok(format!("{login} is already banned for {target}"));
        }
        let list = BanList::from_value(Some(&Value::Object(ban)));
        let elsewhere: Vec<&str> = list
            .rules()
            .iter()
            .filter(|(_, logins)| logins.iter().any(|known| same_login(known, &login)))
            .map(|(key, _)| key.as_str())
            .collect();
        let hint = if elsewhere.is_empty() {
            String::new()
        } else {
            format!(
                "; it is banned for {} (--account {})",
                elsewhere.join(", "),
                elsewhere[0]
            )
        };
        return Err(Refused::Error(format!("{login} is not banned for {target}{hint}")));
    }
    settings.set_autoreview_settings(&[("ban", (!ban.is_empty()).then_some(Value::Object(ban)))]);
    if let Some(error) = settings.drain_errors().into_iter().next() {
        return Err(Refused::Error(format!(
            "{} was not written: {}",
            error.path.display(),
            error.message
        )));
    }
    Ok(if add {
        format!(
            "Banned {login} for {target}: autoreview does not review their pull requests there, nor act on their review requests and mentions"
        )
    } else {
        format!("Unbanned {login} for {target}")
    })
}
