//! `PI_CODING_AGENT` / `AI_AGENT` in every session process: replays fixtures/agent-markers.json (cli/setup.ts
//! `setupCli` and process.ts's `{ ...process.env, ...options.env }`, recorded by fixtures/generate-agent-markers.mjs)
//! through `daemon::process::spawn` with a stand-in executable that writes the environment it got. The spawning
//! process is this test binary re-run with `ULTRON_INTERNAL_EXE` naming the stand-in.

use daemon::process::{AGENT_MARKERS, INTERNAL_EXE_ENV, Role, spawn};
use serde_json::Value;
use std::process::{Command, Stdio};

const OUT_ENV: &str = "DAEMON_AGENT_MARKERS_OUT";
const OPTIONS_ENV: &str = "DAEMON_AGENT_MARKERS_OPTIONS";

fn env_of(text: &str, key: &str) -> Option<String> {
    text.split('\0')
        .find_map(|entry| entry.strip_prefix(&format!("{key}=")).map(str::to_string))
}

/// Run as the spawning process: spawn the internal process with the `options.env` named by the environment and
/// wait for it to end.
#[test]
fn daemon_agent_markers_spawner_process() {
    let Ok(out) = std::env::var(OUT_ENV) else { return };
    let env: Vec<(String, String)> = serde_json::from_str(&std::env::var(OPTIONS_ENV).unwrap()).unwrap();
    let mut child = spawn(Role::Server, &[out], &env).unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn daemon_internal_processes_carry_the_agent_markers_like_ts() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/agent-markers.json")).unwrap();
    let markers: Vec<(String, String)> = AGENT_MARKERS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let want: Vec<(String, String)> = fixture["markers"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    assert_eq!(markers, want);

    let dir = tempfile::tempdir().unwrap();
    // The stand-in "ultron": `__internal <role> <out>` writes its environment to <out>.
    let exe = dir.path().join("fake-ultron");
    std::fs::write(
        &exe,
        "#!/bin/sh\n/usr/bin/env -0 > \"$3.tmp\" && /bin/mv \"$3.tmp\" \"$3\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let run = |name: &str, env: &[(String, String)]| -> String {
        let out = dir.path().join(name);
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "daemon_agent_markers_spawner_process", "--test-threads=1"])
            .env(INTERNAL_EXE_ENV, &exe)
            .env(OUT_ENV, &out)
            .env(OPTIONS_ENV, serde_json::to_string(env).unwrap())
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{name}: the spawning process failed");
        std::fs::read_to_string(out).unwrap()
    };
    let plain = run("plain.env", &[]);
    for (key, value) in &want {
        assert_eq!(env_of(&plain, key).as_deref(), Some(value.as_str()), "{key}");
    }
    // `options.env` still wins over the markers.
    let overridden = run("override.env", &[("AI_AGENT".into(), "from-options".into())]);
    for (key, value) in fixture["withOptionsEnv"].as_object().unwrap() {
        assert_eq!(env_of(&overridden, key).as_deref(), value.as_str(), "{key}");
    }
}
