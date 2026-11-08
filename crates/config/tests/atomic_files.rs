//! Config and credential files are replaced, never truncated in place, and a trust store that cannot be read does
//! not trust a project in a worker.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use serde_json::json;

/// A hard link to the old file keeps the old text when the file is replaced by a rename; a write that truncates and
/// rewrites the same inode changes it under the link (and a crash in between leaves it empty).
fn assert_replaced(path: &Path, write: impl FnOnce()) {
    let dir = path.parent().unwrap();
    let old = dir.join("old-inode");
    fs::hard_link(path, &old).unwrap();
    let before = fs::read_to_string(&old).unwrap();
    write();
    assert_eq!(
        fs::read_to_string(&old).unwrap(),
        before,
        "{} was rewritten in place",
        path.display()
    );
    assert_ne!(fs::metadata(path).unwrap().ino(), fs::metadata(&old).unwrap().ino());
    fs::remove_file(&old).unwrap();
    let leftovers: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
}

#[test]
fn config_write_private_replaces_the_file_by_rename_with_mode_0600() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    fs::write(&path, "{\"a\":1}").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_replaced(&path, || config::paths::write_private(&path, "{\"b\":2}").unwrap());
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"b\":2}");
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn config_write_private_follows_a_symlink_and_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.json");
    let link = dir.path().join("auth.json");
    fs::write(&real, "{}").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    config::paths::write_private(&link, "{\"x\":true}").unwrap();
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_to_string(&real).unwrap(), "{\"x\":true}");
}

#[test]
fn config_a_failed_write_private_leaves_the_old_file_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    fs::write(&path, "{\"keep\":1}").unwrap();
    // The file itself stays writable, so only a write that goes through a new file in the directory fails here.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let result = config::paths::write_private(&path, "{\"new\":2}");
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"keep\":1}");
}

#[test]
fn config_create_private_if_missing_never_replaces_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp-auth.json");
    config::paths::create_private_if_missing(&path, "{}").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    fs::write(&path, "{\"server\":{}}").unwrap();
    config::paths::create_private_if_missing(&path, "{}").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "{\"server\":{}}");
}

#[test]
fn config_auth_trust_and_settings_writes_replace_their_files() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    fs::create_dir_all(&agent).unwrap();
    let paths = config::paths::AgentPaths::new(agent.clone());

    fs::write(paths.auth(), "{\"other\":{\"type\":\"api_key\",\"key\":\"k\"}}").unwrap();
    let mut auth = config::auth::AuthStorage::create(paths.auth());
    assert_replaced(&paths.auth(), || {
        auth.modify("p", |_| Some(json!({"type": "api_key", "key": "x"})))
            .unwrap();
    });

    let project = dir.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(paths.trust(), "{}\n").unwrap();
    let store = config::trust::ProjectTrustStore::new(&agent);
    assert_replaced(&paths.trust(), || store.set(&project, Some(true)).unwrap());
    assert_eq!(store.get(&project).unwrap(), Some(true));

    fs::write(paths.settings(), "{}").unwrap();
    let mut settings = config::settings::SettingsManager::create(&project, &agent, false);
    assert_replaced(&paths.settings(), || settings.set_global("theme", Some(json!("dark"))));
    assert!(settings.drain_errors().is_empty());
}

/// A project with a resource that needs trust, a stored "do not trust" decision, and the default `ask`.
fn distrusted_project(root: &Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let cwd = root.join("work");
    let agent = root.join("agent");
    let home = root.join("home");
    fs::create_dir_all(cwd.join(".pi")).unwrap();
    fs::write(cwd.join(".pi/settings.json"), "{}").unwrap();
    fs::create_dir_all(&agent).unwrap();
    fs::create_dir_all(&home).unwrap();
    config::trust::ProjectTrustStore::new(&agent)
        .set(&cwd, Some(false))
        .unwrap();
    assert!(!config::trust::worker_project_trusted(&cwd, &agent, &home));
    (cwd, agent, home)
}

#[test]
fn config_worker_trust_fails_closed_on_a_torn_trust_store() {
    let dir = tempfile::tempdir().unwrap();
    let (cwd, agent, home) = distrusted_project(dir.path());
    let trust = config::paths::AgentPaths::new(agent.clone()).trust();
    let text = fs::read_to_string(&trust).unwrap();
    fs::write(&trust, &text[..text.len() / 2]).unwrap();
    assert!(!config::trust::worker_project_trusted(&cwd, &agent, &home));
}

#[test]
fn config_worker_trust_fails_closed_while_the_trust_store_lock_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let (cwd, agent, home) = distrusted_project(dir.path());
    let trust = config::paths::AgentPaths::new(agent.clone()).trust();
    let mut lock = trust.into_os_string();
    lock.push(".lock");
    fs::create_dir(&lock).unwrap();
    assert!(!config::trust::worker_project_trusted(&cwd, &agent, &home));
}
