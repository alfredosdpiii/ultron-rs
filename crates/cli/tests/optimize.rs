//! `ultron optimize` through `cli::run`, under a fake HOME: offline judge, memory and autoreview runs keep versioned
//! points and a threshold proposal; `--live` needs a budget and runs only against loopback fakes here (a d1 server
//! in this file, the fake Hindsight server of crates/hindsight/fixtures).

use crate::hindsight_common::FakeHindsight;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap().join("home");
        std::fs::create_dir_all(home.join("work")).unwrap();
        std::fs::create_dir_all(home.join(".ultron/agent")).unwrap();
        Machine { _dir: dir, home }
    }

    fn agent(&self) -> PathBuf {
        self.home.join(".ultron/agent")
    }

    fn env(&self, vars: &[(&str, String)]) -> cli::Env {
        let mut all: Vec<(String, String)> = vec![
            ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
            ("ULTRON_MEMORANTADO_URL".into(), "off".into()),
            ("HOME".into(), self.home.display().to_string()),
            (config::paths::ENV_AGENT_DIR.into(), self.agent().display().to_string()),
            (
                config::paths::ENV_SERVER_DIR.into(),
                self.home.join("server").display().to_string(),
            ),
        ];
        all.extend(vars.iter().map(|(k, v)| (k.to_string(), v.clone())));
        cli::Env {
            cwd: self.home.join("work"),
            home: self.home.clone(),
            var: Arc::new(move |key| all.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
            package: config::paths::normalize(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent"),
            ),
        }
    }

    fn points(&self, pipeline: &str) -> Vec<Value> {
        std::fs::read_to_string(self.agent().join("optimize").join(pipeline).join("points.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

fn run(args: &[&str], env: &cli::Env) -> (i32, String, String) {
    let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(&argv, env, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn cli_optimize_judge_offline_keeps_versioned_points_and_a_threshold_proposal() {
    let machine = Machine::new();
    let env = machine.env(&[]);
    let (code, out, err) = run(&["optimize", "judge"], &env);
    assert_eq!(code, 0, "{out}{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].contains("jev@v1") && lines[0].contains("passed"), "{out}");
    assert!(out.contains("d1@v1") && out.contains("jev-fitted@v1"), "{out}");
    assert!(lines.iter().any(|l| l.starts_with('*')), "a frontier is marked: {out}");
    assert!(lines.last().unwrap().starts_with("Kept in "), "{out}");
    assert!(
        err.contains("fit memory jev:") && err.contains("Threshold proposal (not applied): "),
        "{err}"
    );
    let proposals: Vec<PathBuf> = std::fs::read_dir(machine.agent().join("optimize/judge/proposals"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let proposal: Value = serde_json::from_str(&std::fs::read_to_string(&proposals[0]).unwrap()).unwrap();
    assert_eq!(proposal["proposal"], "judge.thresholds");
    // Nothing reached the settings.
    assert!(!machine.agent().join("settings.json").exists());
    let first = machine.points("judge");
    assert!(
        first
            .iter()
            .all(|p| p["mode"] == "offline" && p["fixtureHash"].is_string())
    );

    // A rerun keeps the versions; a changed variant config is its next version.
    let (code, ..) = run(&["optimize", "judge"], &env);
    assert_eq!(code, 0);
    let file = machine.home.join("work/variants.json");
    std::fs::write(
        &file,
        json!([{"name": "jev", "config": {"provider": ["jev"], "thresholds": {"jev": 0.9}}}]).to_string(),
    )
    .unwrap();
    let (code, out, err) = run(&["optimize", "judge", "--variants", "variants.json", "--json"], &env);
    assert_eq!(code, 0, "{err}");
    let shown: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(shown["points"][0]["variant"], "jev@v2");
    assert_eq!(shown["points"][0]["frontier"], true);
    assert_eq!(
        machine.points("judge").len(),
        first.len() * 2 + 1,
        "points are appended, never rewritten"
    );

    std::fs::write(&file, json!([{"name": "jev", "config": {"budgetUsd": 1}}]).to_string()).unwrap();
    let (code, _, err) = run(&["optimize", "judge", "--variants", "variants.json"], &env);
    assert_eq!(code, 1);
    assert!(err.contains("variant jev: judge variants set"), "{err}");
}

#[test]
fn cli_optimize_refuses_a_live_run_without_a_budget_and_stray_options() {
    let machine = Machine::new();
    let env = machine.env(&[]);
    for (args, expected) in [
        (vec!["optimize", "judge", "--live"], "--live needs --budget"),
        (vec!["optimize", "judge", "--budget", "1"], "apply to --live runs"),
        (vec!["optimize", "judge", "--live", "--budget", "-1"], "positive number"),
        (
            vec!["optimize", "autoreview", "--live", "--budget", "1"],
            "autoreview has no --live",
        ),
        (vec!["optimize", "memory", "extra"], "unexpected argument extra"),
        (vec!["optimize", "memory", "--recording", "x"], "--recording applies"),
    ] {
        let (code, _, err) = run(&args, &env);
        assert_eq!(code, 2, "{args:?}: {err}");
        assert!(err.contains(expected), "{args:?}: {err}");
    }
    let (code, out, _) = run(&["optimize", "--help"], &env);
    assert_eq!(code, 0);
    assert!(out.starts_with("Usage: ultron optimize <pipeline>"));
    // A hindsight variant offline is refused.
    std::fs::write(
        machine.home.join("work/v.json"),
        json!([{"name": "h", "config": {"backend": "hindsight"}}]).to_string(),
    )
    .unwrap();
    let (code, _, err) = run(&["optimize", "memory", "--variants", "v.json"], &env);
    assert_eq!(code, 1);
    assert!(err.contains("runs only with --live"), "{err}");
}

#[test]
fn cli_optimize_memory_offline_and_live_on_a_throwaway_bank() {
    let machine = Machine::new();
    let (code, out, err) = run(&["optimize", "memory"], &machine.env(&[]));
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.contains("reference@v1") && out.contains("reference-min0.5@v1"),
        "{out}"
    );
    let points = machine.points("memory");
    assert_eq!(points[0]["outcome"], "passed");
    assert_eq!(points[0]["costUsd"], 0.0);

    let fake = FakeHindsight::start();
    let env = machine.env(&[("ULTRON_HINDSIGHT_URL", fake.base())]);
    let (code, out, err) = run(&["optimize", "memory", "--live", "--budget", "1"], &env);
    assert_eq!(code, 0, "{out}{err}");
    assert!(err.contains("throwaway bank ultron-optimize-"), "{err}");
    let live = machine.points("memory").pop().unwrap();
    assert_eq!(
        (live["variant"].as_str(), live["mode"].as_str()),
        (Some("hindsight@v1"), Some("live"))
    );
    assert_eq!(live["costUsd"], Value::Null, "the server's own spend is not visible");
    // The set went to the throwaway bank, never to the user's ("ultron").
    let requests = fake.requests(&[]);
    let lines: Vec<&str> = requests
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["line"].as_str())
        .collect();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("DELETE") && l.contains("/documents/p1-journal")),
        "{lines:?}"
    );
    assert!(lines.iter().all(|l| l.contains("/banks/ultron-optimize-")), "{lines:?}");
}

#[test]
fn cli_optimize_autoreview_rescores_reports_at_each_level() {
    let machine = Machine::new();
    let report = Path::new(env!("CARGO_MANIFEST_DIR")).join("../optimize/tests/data/autoreview-report.json");
    let evals = machine.agent().join("autoreview/evals");
    std::fs::create_dir_all(&evals).unwrap();
    std::fs::copy(&report, evals.join("repo-1.json")).unwrap();
    let (code, out, err) = run(&["optimize", "autoreview"], &machine.env(&[]));
    assert_eq!(code, 0, "{out}{err}");
    let names: Vec<&str> = out
        .lines()
        .filter_map(|l| l.get(2..).and_then(|l| l.split_whitespace().next()))
        .collect();
    assert_eq!(
        &names[..5],
        [
            "anthropic-claude-sonnet-5.critical@v1",
            "anthropic-claude-sonnet-5.high@v1",
            "anthropic-claude-sonnet-5.medium@v1",
            "anthropic-claude-sonnet-5.low@v1",
            "anthropic-claude-sonnet-5.nit@v1"
        ]
    );
    let (code, _, err) = run(&["optimize", "autoreview", "missing.json"], &machine.env(&[]));
    assert_eq!(code, 1);
    assert!(err.contains("missing.json"), "{err}");
}

/// A loopback d1 server: every `/decide` answered `supported` (or yes) at 0.95; it serves `answers` requests.
fn d1_server(answers: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(answers) {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            let (name, question) = request["questions"].as_object().unwrap().iter().next().unwrap();
            let answer = match question["type"].as_str().unwrap() {
                "noul" => json!({"noul": 0.95}),
                _ => json!({"choice": "supported", "confidence": 0.95}),
            };
            let reply = json!({"answers": {name.clone(): answer}}).to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            )
            .unwrap();
        }
    });
    port
}

#[test]
fn cli_optimize_judge_live_records_under_a_ceiling_and_replays_the_recording() {
    let machine = Machine::new();
    let port = d1_server(4);
    std::fs::write(
        machine.agent().join("settings.json"),
        json!({"judge": {"d1": {"url": format!("http://127.0.0.1:{port}")}}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        machine.home.join("work/v.json"),
        json!([{"name": "d1", "config": {"provider": ["d1"]}}]).to_string(),
    )
    .unwrap();
    let env = machine.env(&[("ULTRON_D1_KEY", ["fake", "-d1-", "key"].concat())]);
    let args = [
        "optimize",
        "judge",
        "--live",
        "--budget",
        "1",
        "--max-calls",
        "4",
        "--variants",
        "v.json",
    ];
    let (code, out, err) = run(&args, &env);
    assert_eq!(code, 0, "{out}{err}");
    assert!(err.contains("stopped: the judge's call ceiling (4 calls"), "{err}");
    let recordings = machine.agent().join("optimize/judge/recordings");
    let path = std::fs::read_dir(&recordings).unwrap().next().unwrap().unwrap().path();
    let recording: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(recording["calls"].as_array().unwrap().len(), 4);
    assert!(
        !recording.to_string().contains("fake-d1-key"),
        "keys are never recorded"
    );
    let point = machine.points("judge").pop().unwrap();
    assert_eq!(point["mode"], "live");
    // The fake says "supported" to everything: two of the four recorded memory cases are accepted wrong; the cases
    // past the ceiling have no answer.
    assert_eq!(point["outcome"], "failed", "{point}");
    assert_eq!(
        (
            point["metrics"]["wrong"].as_u64(),
            point["metrics"]["abstained"].as_u64()
        ),
        (Some(2), Some(31))
    );

    // The saved recording replays offline.
    let (code, out, err) = run(
        &[
            "optimize",
            "judge",
            "--recording",
            path.to_str().unwrap(),
            "--variants",
            "v.json",
        ],
        &env,
    );
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("d1@v1") && out.contains("failed"), "{out}");
}
