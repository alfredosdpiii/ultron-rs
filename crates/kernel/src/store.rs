//! One JSON document replaced whole on every write (`HostModuleStore`): the host modules' documents, the shell job
//! journal, the task journal, and the typed agents' definitions and usage ledger.

use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub trait DocumentStore: Send + Sync {
    /// The stored document, or `None` before the first write.
    fn read(&self) -> Result<Option<Value>, String>;
    fn write(&self, document: &Value) -> Result<(), String>;
}

impl<T: DocumentStore + ?Sized> DocumentStore for Box<T> {
    fn read(&self) -> Result<Option<Value>, String> {
        (**self).read()
    }

    fn write(&self, document: &Value) -> Result<(), String> {
        (**self).write(document)
    }
}

impl<T: DocumentStore + ?Sized> DocumentStore for Arc<T> {
    fn read(&self) -> Result<Option<Value>, String> {
        (**self).read()
    }

    fn write(&self, document: &Value) -> Result<(), String> {
        (**self).write(document)
    }
}

/// A document kept in memory (`createMemoryModuleStore`: a session without a journal, and tests).
#[derive(Default)]
pub struct MemoryDocumentStore {
    document: Mutex<Option<Value>>,
}

impl MemoryDocumentStore {
    pub fn new(initial: Option<Value>) -> Self {
        Self {
            document: Mutex::new(initial),
        }
    }
}

impl DocumentStore for MemoryDocumentStore {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(jsrt::sync::lock(&self.document).clone())
    }

    fn write(&self, document: &Value) -> Result<(), String> {
        *jsrt::sync::lock(&self.document) = Some(document.clone());
        Ok(())
    }
}

/// A JSON file replaced atomically (temporary file, then rename) on every write.
pub struct FileDocumentStore {
    path: PathBuf,
}

impl FileDocumentStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl DocumentStore for FileDocumentStore {
    fn read(&self) -> Result<Option<Value>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|error| format!("{}: {error}", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", self.path.display())),
        }
    }

    fn write(&self, document: &Value) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let temporary = PathBuf::from(format!("{}.{}.tmp", self.path.display(), jsrt::random::uuid_v4()));
        std::fs::write(&temporary, serde_json::to_string(document).unwrap_or_default())
            .map_err(|error| error.to_string())?;
        std::fs::rename(&temporary, &self.path).map_err(|error| error.to_string())
    }
}
