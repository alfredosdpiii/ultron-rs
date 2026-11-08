//! Keyed durable documents (`DurableDocumentStorage` of `ultron/local-services.ts`), in memory or in one JSON file:
//! the store of schedules, local services and prompt schedules. (TypeScript's `artifacts.*` host requests over the
//! same store are not ported.)

use kernel::store::DocumentStore;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Mutex;

/// Keyed durable documents (`DurableDocumentStorage`): listings keep insertion order.
pub trait KeyedDocuments: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Value>, String>;
    fn set(&self, key: &str, value: &Value) -> Result<(), String>;
    /// Every `(key, value)` whose key starts with `prefix`, in insertion order.
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String>;
}

/// In-memory documents (a JavaScript `Map`).
#[derive(Default)]
pub struct MemoryKeyedDocuments {
    values: Mutex<Map<String, Value>>,
}

impl KeyedDocuments for MemoryKeyedDocuments {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        Ok(jsrt::sync::lock(&self.values).get(key).cloned())
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        jsrt::sync::lock(&self.values).insert(key.into(), value.clone());
        Ok(())
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        Ok(jsrt::sync::lock(&self.values)
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect())
    }
}

/// Documents in one JSON file (`{key: value}` in insertion order), replaced atomically on every write.
pub struct FileKeyedDocuments {
    store: kernel::store::FileDocumentStore,
    guard: Mutex<()>,
}

impl FileKeyedDocuments {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileKeyedDocuments {
            store: kernel::store::FileDocumentStore::new(path),
            guard: Mutex::new(()),
        }
    }

    fn read(&self) -> Result<Map<String, Value>, String> {
        match self.store.read()? {
            None => Ok(Map::new()),
            Some(Value::Object(map)) => Ok(map),
            Some(_) => Err("Invalid artifact document file".into()),
        }
    }
}

impl KeyedDocuments for FileKeyedDocuments {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        let _guard = jsrt::sync::lock(&self.guard);
        Ok(self.read()?.get(key).cloned())
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        let _guard = jsrt::sync::lock(&self.guard);
        let mut map = self.read()?;
        map.insert(key.into(), value.clone());
        self.store.write(&Value::Object(map))
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        let _guard = jsrt::sync::lock(&self.guard);
        Ok(self
            .read()?
            .into_iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .collect())
    }
}
