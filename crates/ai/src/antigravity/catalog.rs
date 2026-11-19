//! Account-specific picker inventory from `agy models`. Explicit IDs do not require discovery.
use crate::models::antigravity_model;
use crate::types::Model;
use signals::child::{self, Bound, Stop};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Snapshot {
    executable: PathBuf,
    home: PathBuf,
    expires: Instant,
    models: Vec<Model>,
}

pub fn discover(path: &str, home: &Path) -> Vec<Model> {
    static CACHE: Mutex<Option<Snapshot>> = Mutex::new(None);
    let Some(executable) = jsrt::process::which("agy", path) else {
        return vec![];
    };
    let mut cache = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hit) = cache
        .as_ref()
        .filter(|hit| hit.home == home && hit.executable == executable && Instant::now() < hit.expires)
    {
        return hit.models.clone();
    }
    let mut command = Command::new(&executable);
    command
        .arg("models")
        .env("PATH", path)
        .env("HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let bound = Bound {
        timeout: Some(Duration::from_secs(4)),
        keep: Some(256 * 1024),
        stop: Stop {
            group: true,
            term_grace: None,
        },
    };
    let models = child::run(&mut command, None, bound)
        .ok()
        .filter(|out| out.success() && !out.cut)
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|tsv| parse(&tsv))
        .unwrap_or_default();
    let ttl = Duration::from_secs(if models.is_empty() { 15 } else { 300 });
    *cache = Some(Snapshot {
        executable,
        home: home.into(),
        expires: Instant::now() + ttl,
        models: models.clone(),
    });
    models
}

fn parse(tsv: &str) -> Vec<Model> {
    let mut seen = std::collections::HashSet::new();
    tsv.lines()
        .filter_map(|line| {
            let (id, name) = line.trim().split_once('\t')?;
            let mut model = antigravity_model(id)?;
            if name.trim().is_empty() || !seen.insert(id) {
                return None;
            }
            model.name = name.trim().into();
            Some(model)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn antigravity_preserves_advertised_ids_and_labels_once() {
        let models = super::parse(
            "Fetching...\nfirst-id\tFriendly First\nfirst-id\tDuplicate\nbad id\tBad\nnew-id\tNew Model\n",
        );
        assert_eq!(
            models
                .iter()
                .map(|m| (m.id.as_str(), m.name.as_str()))
                .collect::<Vec<_>>(),
            [("first-id", "Friendly First"), ("new-id", "New Model")]
        );
    }
}
