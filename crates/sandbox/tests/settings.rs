//! `sandbox.*` settings, the interactive policy built from them, and write containment.

use sandbox::launcher::{Entry, Layout, Mechanism, Policy};
use sandbox::settings::{DEFAULT_HIDDEN, DEFAULT_HOME_WRITABLE, Mode, Settings};
use serde_json::json;
use std::path::{Path, PathBuf};

#[test]
fn sandbox_settings_defaults_and_overrides() {
    let defaults = Settings::from_settings(&json!({}));
    assert_eq!(defaults.mode, Mode::Off);
    assert!(defaults.network);
    assert_eq!(defaults.writable.len(), DEFAULT_HOME_WRITABLE.len());
    assert_eq!(defaults.hidden.len(), DEFAULT_HIDDEN.len());
    assert!(defaults.hidden.contains(&"~/.ssh".to_owned()));
    let custom = Settings::from_settings(&json!({ "sandbox": {
        "mode": " Required ", "network": false, "writable": ["~/w", 3, ""], "hidden": [], "env": ["FOO"], "image": "img:1"
    }}));
    assert_eq!(custom.mode, Mode::Required);
    assert!(!custom.network);
    assert_eq!(custom.writable, vec!["~/w".to_owned()]);
    assert!(custom.hidden.is_empty());
    assert_eq!(custom.env, vec!["FOO".to_owned()]);
    assert_eq!(custom.image.as_deref(), Some("img:1"));
    let invalid = Settings::from_settings(&json!({ "sandbox": { "mode": "sometimes", "network": "no" } }));
    assert_eq!(invalid.mode, Mode::Off);
    assert!(invalid.network);
}

fn scratch() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("sandbox-settings-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let home = base.join("home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(home.join(".cache")).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    (dir, home, project)
}

#[test]
fn sandbox_policy_paths_env_and_write_containment() {
    let (_dir, home, project) = scratch();
    let jobs = project.join(".jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    let env: Vec<(String, String)> = [
        ("PATH", "/usr/bin"),
        ("HOME", "/h"),
        ("LC_ALL", "C"),
        ("GITHUB_TOKEN", "t"),
        ("FOO", "f"),
        ("TMPDIR", "/x"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let settings = Settings {
        env: vec!["FOO".into()],
        ..Default::default()
    };
    let policy = Policy::new(
        &settings,
        &project,
        &home,
        &[jobs.clone(), home.join("missing")],
        None,
        &env,
    );
    assert_eq!(policy.writable, vec![project.clone(), jobs, home.join(".cache")]);
    assert_eq!(policy.hidden, vec![home.join(".ssh")]);
    let names: Vec<&str> = policy.env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["PATH", "HOME", "LC_ALL", "FOO", "TMPDIR"]);
    assert_eq!(policy.env.last().unwrap().1, "/tmp");

    assert!(policy.may_write(&project.join("a/b.txt")));
    assert!(policy.may_write(&home.join(".cache/x")));
    assert!(!policy.may_write(&home.join("x")));
    assert!(!policy.may_write(&project.join("../x")));
    assert!(!policy.may_write(Path::new("/etc/passwd")));
    std::os::unix::fs::symlink(&home, project.join("up")).unwrap();
    assert!(!policy.may_write(&project.join("up/x")));
    // Hidden wins even under a writable path.
    let mut wide = policy.clone();
    wide.writable.push(home.clone());
    assert!(wide.may_write(&home.join("x")));
    assert!(!wide.may_write(&home.join(".ssh/authorized_keys")));
}

struct Flat;
impl Layout for Flat {
    fn entry(&self, path: &Path) -> Entry {
        match path.to_str().unwrap() {
            "/usr" | "/lib" | "/h" | "/p" | "/h/.ssh" | "/t" => Entry::Dir,
            "/bin" => Entry::Symlink("usr/bin".into()),
            "/h/.netrc" => Entry::File,
            _ => Entry::Missing,
        }
    }
    fn which(&self, name: &str) -> bool {
        name == "setpriv"
    }
}

fn flat_policy(network: bool, tmp: Option<&str>) -> Policy {
    Policy {
        cwd: "/p".into(),
        home: "/h".into(),
        writable: vec!["/p".into(), "/gone".into()],
        readable: vec![],
        hidden: vec!["/h/.ssh".into(), "/h/.netrc".into()],
        tmp: tmp.map(PathBuf::from),
        network,
        env: vec![("PATH".into(), "/usr/bin".into())],
        image: Some("img".into()),
        uid: 7,
        gid: 8,
        launch: Vec::new(),
    }
}

fn argv(policy: &Policy, mechanism: Mechanism) -> Vec<String> {
    policy.wrap(
        mechanism,
        &Flat,
        &["bash".into(), "-c".into(), "x".into()],
        Path::new("/p"),
    )
}

/// The index just past `fragment` (consecutive words of `argv`) at or after `from`, or a panic naming it.
fn after(argv: &[String], from: usize, fragment: &str) -> usize {
    let words: Vec<&str> = fragment.split(' ').collect();
    (from..=argv.len().saturating_sub(words.len()))
        .find(|&at| argv[at..at + words.len()].iter().zip(&words).all(|(a, b)| a == b))
        .map(|at| at + words.len())
        .unwrap_or_else(|| panic!("`{fragment}` not found after word {from} of {}", argv.join(" ")))
}

/// `fragments` occur in `argv` in this order: later mounts cover earlier ones, so the order is the policy.
fn in_order(argv: &[String], fragments: &[&str]) {
    fragments.iter().fold(0, |from, fragment| after(argv, from, fragment));
}

#[test]
fn sandbox_interactive_bwrap_argv() {
    let off = argv(&flat_policy(false, None), Mechanism::Bwrap);
    in_order(
        &off,
        &[
            "bwrap --unshare-all",
            "--die-with-parent --new-session --cap-drop ALL --clearenv --setenv PATH /usr/bin",
            "--ro-bind /usr /usr",
            "--tmpfs /tmp",
            "--ro-bind /h /h",
            "--bind /p /p",
            "--tmpfs /h/.ssh",
            "--ro-bind /dev/null /h/.netrc",
            "--chdir /p -- bash -c x",
        ],
    );
    assert!(!off.contains(&"--share-net".to_owned()), "{off:?}");
    assert!(!off.iter().any(|word| word == "/gone"), "{off:?}");
    let on = argv(&flat_policy(true, Some("/t")), Mechanism::Bwrap);
    in_order(
        &on,
        &[
            "bwrap --unshare-all --share-net",
            "--ro-bind-try /run/systemd/resolve /run/systemd/resolve",
            "--bind /t /tmp",
            "-- bash -c x",
        ],
    );
    assert!(!on.join(" ").contains("--tmpfs /tmp"), "{on:?}");
}

#[test]
fn sandbox_interactive_unshare_and_docker_argv() {
    let off = argv(&flat_policy(false, None), Mechanism::Unshare);
    in_order(
        &off,
        &[
            "setpriv --pdeathsig KILL -- unshare --user --map-root-user --mount --net --pid --fork",
            "--kill-child env",
            "/h",
            "/p",
            "/h/.ssh /h/.netrc",
            "-- bash -c x",
        ],
    );
    let on = argv(&flat_policy(true, None), Mechanism::Unshare);
    assert!(!on.contains(&"--net".to_owned()), "{on:?}");
    let docker = argv(&flat_policy(false, Some("/t")), Mechanism::Docker);
    // The client dies with its parent, and the container with its client (escape.rs runs both for real).
    in_order(
        &docker,
        &[
            "setpriv --pdeathsig KILL -- bash -c",
            "ultron-docker docker run --rm --init -i --pull never --network none --cap-drop ALL \
--security-opt no-new-privileges",
            "--user 7:8",
            "-v /t:/tmp:rw",
            "-v /h:/h:ro",
            "-v /p:/p:rw",
            "--tmpfs /h/.ssh",
            "-v /dev/null:/h/.netrc:ro",
            "-w /p img",
            "env -i PATH=/usr/bin bash -c x",
        ],
    );
    assert_eq!(docker.last().map(String::as_str), Some("x"));
}

/// A writable or readable path inside a hidden one is mounted again after the hidden ones (most specific wins), and
/// `may_write` agrees: the session dir under a hidden agent dir stays usable while its siblings stay hidden.
#[test]
fn sandbox_nested_paths_inside_hidden_ones_are_mounted_last() {
    let policy = Policy {
        writable: vec!["/p".into(), "/h/.ssh/session/deep".into(), "/h/.ssh/session".into()],
        readable: vec!["/h/.ssh/skills".into()],
        ..flat_policy(false, None)
    };
    struct Nested;
    impl Layout for Nested {
        fn entry(&self, path: &Path) -> Entry {
            match path.to_str().unwrap() {
                "/h/.ssh/session" | "/h/.ssh/session/deep" | "/h/.ssh/skills" => Entry::Dir,
                _ => Flat.entry(path),
            }
        }
        fn which(&self, name: &str) -> bool {
            Flat.which(name)
        }
    }
    for mechanism in [Mechanism::Bwrap, Mechanism::Unshare] {
        let argv = policy.wrap(mechanism, &Nested, &["true".into()], Path::new("/p"));
        in_order(
            &argv,
            &[
                "/h/.ssh",
                "/h/.netrc",
                "/h/.ssh/skills",
                "/h/.ssh/session",
                "/h/.ssh/session/deep",
                "-- true",
            ],
        );
    }
    assert!(policy.may_write(Path::new("/h/.ssh/session/x")));
    assert!(policy.may_write(Path::new("/h/.ssh/session/deep/x")));
    assert!(!policy.may_write(Path::new("/h/.ssh/id_rsa")));
    assert!(!policy.may_write(Path::new("/h/.ssh/skills/x")));
    assert!(!policy.may_write(Path::new("/h/.netrc")));
}
