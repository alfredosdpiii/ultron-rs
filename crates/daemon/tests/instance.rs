//! `daemon::instance`: which recorded server generations count as running, and the server options of an instance.

/// A generation whose process exists but belongs to another user (`kill(pid, 0)` fails with EPERM, as for pid 1
/// under any user but root) is running; it used to be dropped as dead.
#[test]
fn daemon_instance_generation_of_another_users_process_is_running() {
    let dir = tempfile::tempdir().unwrap();
    let record = |pid: u32| {
        serde_json::json!({"pid": pid, "serverId": format!("s{pid}"), "sessionDir": dir.path().join("sessions")})
            .to_string()
    };
    std::fs::write(dir.path().join("generation-a.json"), record(1)).unwrap();
    std::fs::write(dir.path().join("generation-b.json"), record(0)).unwrap();
    let running = daemon::instance::generations(dir.path());
    let pids: Vec<u32> = running.iter().map(|r| r.pid).collect();
    assert_eq!(pids, [1]);
}

/// Every command's server options come from one constructor: the instance prepared, its root recorded, held open,
/// no relay, and the server ID `ULTRON_SERVER_ID` asks for. `PI_SERVER_ID` is not read: TypeScript's
/// `startForegroundServer` and `activateServer` resolve `ENV_SERVER_ID` only (print mode used to fall back to it).
#[test]
fn daemon_server_options_for_an_instance_are_built_once() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let instance = daemon::instance::Instance {
        kind: daemon::instance::Kind::Project { root: root.clone() },
        dir: daemon::instance::project_dir(home.path(), &root),
    };
    let id = "0b9e4f3c-0d7a-4c55-9a43-2d8f1b6f2a10";
    let var = |name: &str| match name {
        daemon::server::SERVER_ID_ENV => Some(id.to_string()),
        "PI_SERVER_ID" => Some("ignored".to_string()),
        _ => None,
    };
    let sessions = home.path().join("sessions");
    let options =
        daemon::server::ServerOptions::for_instance(&instance, sessions.clone(), &var, Default::default()).unwrap();
    assert_eq!(options.directory, instance.dir);
    assert_eq!(options.session_dir, sessions);
    assert_eq!(options.server_id.as_deref(), Some(id));
    assert_eq!(options.root.as_deref(), Some(root.as_path()));
    assert!(options.keep_alive && options.relay.is_none());
    assert!(instance.dir.join("project.json").is_file());
    let without = daemon::server::ServerOptions::for_instance(&instance, sessions, &|_| None, Default::default());
    assert_eq!(without.unwrap().server_id, None);
}

/// What the socket test below made outside its own temp directory: fallback socket directories, and their parent
/// when the test created it (a live server may already keep sockets in it).
#[derive(Default)]
struct Fallbacks {
    dirs: Vec<std::path::PathBuf>,
    created_parents: Vec<std::path::PathBuf>,
}

impl Drop for Fallbacks {
    fn drop(&mut self) {
        for dir in &self.dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
        for parent in &self.created_parents {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// A server directory whose longest socket path is just over the platform's `sun_path` limit used to keep its
/// sockets, because 107 bytes was assumed everywhere (macOS and the BSDs take 103), and `bind` then failed with
/// `path must be shorter than SUN_LEN`. At every length across both limits the directory `socket_dir` returns must
/// take the longest socket name.
#[test]
fn daemon_instance_socket_dir_takes_the_longest_socket_name_at_every_length() {
    let base = tempfile::Builder::new().prefix("us").tempdir_in("/tmp").unwrap();
    let name = protocol::sockets::generation_name("0b9e4f3c-0d7a-4c55-9a43-2d8f1b6f2a10", "0123456789ab");
    assert_eq!(name.len(), protocol::sockets::LONGEST_NAME);
    let mut fallbacks = Fallbacks::default();
    for length in 35..=50 {
        let padding = length - base.path().as_os_str().len() - 1;
        let server_dir = base.path().join("d".repeat(padding));
        assert_eq!(server_dir.as_os_str().len(), length);
        let sockets = daemon::instance::socket_dir(&server_dir);
        if sockets != server_dir {
            let parent = sockets.parent().unwrap().to_path_buf();
            if !parent.exists() {
                fallbacks.created_parents.push(parent);
            }
            fallbacks.dirs.push(sockets.clone());
        }
        std::fs::create_dir_all(&sockets).unwrap();
        let path = sockets.join(&name);
        if let Err(error) = std::os::unix::net::UnixListener::bind(&path) {
            panic!("server directory of {length} bytes: bind {}: {error}", path.display());
        }
    }
}
