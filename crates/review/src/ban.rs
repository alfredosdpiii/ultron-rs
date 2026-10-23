//! `autoreview.ban`: per reviewing account, the GitHub logins whose pull requests it never reviews and whose
//! review requests and mentions it does not act on. A global setting only (a repository cannot change it):
//!
//! ```json
//! { "autoreview": { "ban": { "bryandlp-boost": ["lepmoran-boostkh"], "*": ["spammer"] } } }
//! ```
//!
//! A key is a reviewing account's login, its `host/login` ([`crate::accounts::account_key`]), or `*` for every
//! account. Keys and logins compare case-insensitively, as GitHub logins do.
//!
//! The daemon re-reads the list every poll and before each decision and model turn ([`Bans::refresh`]), so a ban
//! takes effect without a restart.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::accounts::{Account, account_key};

/// The key that bans a login for every account.
pub const ALL: &str = "*";

/// The start of a skip reason for a banned author ([`banned_reason`]).
pub const BANNED: &str = "author banned: ";

/// The parsed `autoreview.ban`: `(key, logins)` as written, in the file's order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BanList {
    rules: Vec<(String, Vec<String>)>,
}

/// Whether two GitHub logins (or account keys) name the same one.
pub fn same_login(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// Whether a ban key names this reviewing account: `*`, its login, or its `host/login`.
pub fn key_matches(key: &str, account: &Account) -> bool {
    key == ALL || same_login(key, &account.login) || same_login(key, &account_key(account))
}

impl BanList {
    /// From the settings value (an object of arrays of logins); anything else in it is ignored.
    pub fn from_value(value: Option<&Value>) -> Self {
        let mut list = BanList::default();
        let Some(Value::Object(ban)) = value else {
            return list;
        };
        for (key, logins) in ban {
            let key = key.trim();
            let Some(logins) = logins.as_array() else { continue };
            for login in logins.iter().filter_map(Value::as_str) {
                list.add(key, login);
            }
        }
        list
    }

    /// Add `login` under `key` (the key matched case-insensitively); false when it was already there or either is
    /// empty.
    fn add(&mut self, key: &str, login: &str) -> bool {
        let login = normalize_login(login);
        if key.is_empty() || login.is_empty() {
            return false;
        }
        let index = match self.rules.iter().position(|(known, _)| same_login(known, key)) {
            Some(index) => index,
            None => {
                self.rules.push((key.to_string(), Vec::new()));
                self.rules.len() - 1
            }
        };
        let logins = &mut self.rules[index].1;
        if logins.iter().any(|known| same_login(known, &login)) {
            return false;
        }
        logins.push(login);
        true
    }

    pub fn is_empty(&self) -> bool {
        self.rules.iter().all(|(_, logins)| logins.is_empty())
    }

    /// `(key, logins)` as written.
    pub fn rules(&self) -> &[(String, Vec<String>)] {
        &self.rules
    }

    /// Whether any ban applies to this reviewing account.
    pub fn applies_to(&self, account: &Account) -> bool {
        self.rules
            .iter()
            .any(|(key, logins)| !logins.is_empty() && key_matches(key, account))
    }

    /// The key under which `login` is banned for this reviewing account, if it is.
    pub fn banned(&self, account: &Account, login: &str) -> Option<&str> {
        if login.is_empty() {
            return None;
        }
        self.rules
            .iter()
            .find(|(key, logins)| key_matches(key, account) && logins.iter().any(|known| same_login(known, login)))
            .map(|(key, _)| key.as_str())
    }
}

/// A login as typed: trimmed, without a leading `@`.
pub fn normalize_login(text: &str) -> String {
    let text = text.trim();
    text.strip_prefix('@').unwrap_or(text).trim().to_string()
}

/// A GitHub login: letters, digits and hyphens (a bot's ends in `[bot]`).
pub fn valid_login(login: &str) -> bool {
    let name = login.strip_suffix("[bot]").unwrap_or(login);
    !name.is_empty() && name.len() <= 39 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A ban key: `*`, a login, or `host/login`.
pub fn valid_account_key(key: &str) -> bool {
    if key == ALL {
        return true;
    }
    match key.split_once('/') {
        Some((host, login)) => {
            !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
                && valid_login(login)
        }
        None => valid_login(key),
    }
}

/// Add `login` to `ban[key]` (an existing key matched case-insensitively). False when it was already banned there.
/// A value of the wrong type under the key is replaced; other keys are kept as written.
pub fn add_ban(ban: &mut Map<String, Value>, key: &str, login: &str) -> bool {
    let known = ban.keys().find(|known| same_login(known, key)).cloned();
    let entry = ban
        .entry(known.unwrap_or_else(|| key.to_string()))
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entry.is_array() {
        *entry = Value::Array(Vec::new());
    }
    let Value::Array(logins) = entry else {
        return false;
    };
    if logins
        .iter()
        .filter_map(Value::as_str)
        .any(|known| same_login(known, login))
    {
        return false;
    }
    logins.push(Value::from(login));
    true
}

/// Remove `login` from `ban[key]` (both matched case-insensitively); a key left with no login is removed. False
/// when it was not banned there.
pub fn remove_ban(ban: &mut Map<String, Value>, key: &str, login: &str) -> bool {
    let Some(known) = ban.keys().find(|known| same_login(known, key)).cloned() else {
        return false;
    };
    let Some(Value::Array(logins)) = ban.get_mut(&known) else {
        return false;
    };
    let before = logins.len();
    logins.retain(|item| !item.as_str().is_some_and(|item| same_login(item, login)));
    let removed = logins.len() != before;
    if logins.is_empty() {
        ban.shift_remove(&known);
    }
    removed
}

/// The skip reason for a pull request whose author is banned for the reviewing account.
pub fn banned_reason(author: &str) -> String {
    format!("{BANNED}{author}")
}

/// The author a [`banned_reason`] names.
pub fn banned_author(reason: &str) -> Option<&str> {
    reason.strip_prefix(BANNED).filter(|author| !author.is_empty())
}

/// What a ban key reads as in messages.
pub fn describe_key(key: &str) -> String {
    if key == ALL {
        "every account (*)".into()
    } else {
        key.to_string()
    }
}

/// Why `ultron autoreview review` refuses a banned author's pull request (`name`: `<pull> as <login>`).
pub fn refusal(name: &str, author: &str, rule: &str) -> String {
    let account = if rule == ALL {
        String::new()
    } else {
        format!(" --account {rule}")
    };
    format!(
        "{name}: not reviewed: its author {author} is banned for {} (autoreview.ban; \"ultron autoreview unban {author}{account}\" lifts it)",
        describe_key(rule)
    )
}

/// Reads the current list (the command line reads the global settings file); `None` when it cannot be read, and the
/// last list read stays in force (a settings file caught mid-edit lifts no ban).
pub type BanLoader = Arc<dyn Fn() -> Option<BanList> + Send + Sync>;

/// The ban list the reviewer checks: fixed, or re-read by [`Bans::refresh`].
pub struct Bans {
    current: Mutex<BanList>,
    load: Option<BanLoader>,
}

impl Default for Bans {
    fn default() -> Self {
        Self::fixed(BanList::default())
    }
}

impl Bans {
    pub fn fixed(list: BanList) -> Self {
        Self {
            current: Mutex::new(list),
            load: None,
        }
    }

    /// Read now and again on every [`Bans::refresh`].
    pub fn reloading(load: BanLoader) -> Self {
        Self {
            current: Mutex::new(load().unwrap_or_default()),
            load: Some(load),
        }
    }

    /// The list as last read.
    pub fn get(&self) -> BanList {
        self.current.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Read the list again (a fixed one stays as it is) and return it.
    pub fn refresh(&self) -> BanList {
        let Some(load) = &self.load else {
            return self.get();
        };
        let Some(list) = load() else {
            return self.get();
        };
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = list.clone();
        list
    }
}
