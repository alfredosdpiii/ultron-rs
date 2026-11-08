//! Where optimizer runs are kept, under `<agent dir>/optimize/<pipeline>/`:
//!
//! - `variants.json`: `{name: [{version, config, createdAt}]}`. A variant run with a config unlike its latest
//!   version's becomes the next version; the earlier versions stay.
//! - `points.jsonl`: one [`crate::Point`] record per run, appended, never rewritten (failed runs included).
//! - `proposals/`: fitted settings for a person to apply (never applied here).
//! - `recordings/`: live judge answers, replayable offline with `--recording`.

use crate::point::Point;
use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Store {
    dir: PathBuf,
}

/// A variant name: `^[a-z0-9][a-z0-9._+-]{0,63}$`.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '+' | '-'))
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{}: {e}", path.display())
}

impl Store {
    /// The store under `dir` (`<agent dir>/optimize`).
    pub fn new(dir: impl Into<PathBuf>) -> Store {
        Store { dir: dir.into() }
    }

    fn pipeline(&self, pipeline: &str) -> Result<PathBuf, String> {
        let dir = self.dir.join(pipeline);
        std::fs::create_dir_all(&dir).map_err(io(&dir))?;
        Ok(dir)
    }

    /// The version of variant `name` with `config`: its latest version when the config is the same, else a new one.
    pub fn variant(&self, pipeline: &str, name: &str, config: &Value, at: &str) -> Result<u64, String> {
        if !valid_name(name) {
            return Err(format!(
                "variant name {name:?} must be lowercase letters, digits and . _ + - (at most 64)"
            ));
        }
        let path = self.pipeline(pipeline)?.join("variants.json");
        let mut all: Map<String, Value> = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(e) => return Err(io(&path)(e)),
        };
        let versions = all
            .entry(name)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("{}: {name} is not a list", path.display()))?;
        if let Some(latest) = versions.last()
            && latest["config"] == *config
        {
            return latest["version"]
                .as_u64()
                .ok_or_else(|| format!("{}: {name} has no version", path.display()));
        }
        let version = versions.len() as u64 + 1;
        versions.push(json!({"version": version, "config": config, "createdAt": at}));
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, format!("{:#}\n", Value::Object(all))).map_err(io(&temp))?;
        std::fs::rename(&temp, &path).map_err(io(&path))?;
        Ok(version)
    }

    /// Append `point`'s record.
    pub fn record(&self, point: &Point, at: &str) -> Result<(), String> {
        let path = self.pipeline(point.pipeline)?.join("points.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io(&path))?;
        writeln!(file, "{}", point.to_json(at)).map_err(io(&path))
    }

    /// Every stored point record of `pipeline`, oldest first.
    pub fn points(&self, pipeline: &str) -> Result<Vec<Value>, String> {
        let path = self.dir.join(pipeline).join("points.jsonl");
        match std::fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).map_err(|e| format!("{}: {e}", path.display())))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(io(&path)(e)),
        }
    }

    /// Write `document` as `<pipeline>/<kind>/<stamp>.json` (`kind`: proposals or recordings).
    pub fn write(&self, pipeline: &str, kind: &str, stamp: &str, document: &Value) -> Result<PathBuf, String> {
        let dir = self.pipeline(pipeline)?.join(kind);
        std::fs::create_dir_all(&dir).map_err(io(&dir))?;
        let path = dir.join(format!("{stamp}.json"));
        std::fs::write(&path, format!("{document:#}\n")).map_err(io(&path))?;
        Ok(path)
    }
}
