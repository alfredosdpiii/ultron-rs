//! `<agent>/models-store.json` (`models-store.ts`): provider id to stored model catalog entry, sharing the
//! `auth.json` file backend (lock, 0600 mode, `{}` when missing).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::auth::{parse_data, with_locked_file};

pub struct ModelsStore {
    path: PathBuf,
}

impl ModelsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        ModelsStore { path: path.into() }
    }

    /// The store of the agent directory whose `models.json` is `models_json`.
    pub fn beside(models_json: &Path) -> Self {
        ModelsStore::new(models_json.with_file_name("models-store.json"))
    }

    /// Every stored entry as the file has it now, read without the lock: a missing or malformed file is empty, so a
    /// registry restoring its catalogs never fails on (or creates) the store.
    pub fn snapshot(&self) -> Map<String, Value> {
        match std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok())
        {
            Some(Value::Object(stored)) => stored,
            _ => Map::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self, provider: &str) -> Result<Option<Value>, String> {
        with_locked_file(&self.path, |content| {
            Ok((parse_data(content)?.get(provider).cloned(), None))
        })
    }

    /// Store `entry` for `provider`, keeping every other provider as the file has it now.
    pub fn write(&self, provider: &str, entry: Value) -> Result<(), String> {
        with_locked_file(&self.path, |content| {
            let mut data = parse_data(content)?;
            data.insert(provider.to_string(), entry);
            Ok(((), Some(jsrt::json::stringify_pretty(&Value::Object(data)))))
        })
    }

    pub fn delete(&self, provider: &str) -> Result<(), String> {
        with_locked_file(&self.path, |content| {
            let mut data = parse_data(content)?;
            data.shift_remove(provider);
            Ok(((), Some(jsrt::json::stringify_pretty(&Value::Object(data)))))
        })
    }
}
