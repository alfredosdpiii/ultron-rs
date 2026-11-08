//! Replays crates/kernel-apis/fixtures/goal.json, recorded by fixtures/generate-goal.mjs from the TypeScript
//! GoalDriver (ultron/goal.ts) served by NativeRlmHost to the real runtime.py: the same cells (model `goal.*` calls,
//! the person's `/goal` commands, task-end events and calls from other lanes) run through crates/kernel with the Rust
//! `GoalDriver`, with the same scripted worker (checks, job starts and stops), and must print the same and leave the
//! same goal document and worker calls. Plus the pure functions over the recorded table, and ported TS tests.

use crate::common;

use common::{FIXED_NOW, Made, fixed, fixture, run_scenario};
use kernel::store::{DocumentStore, MemoryDocumentStore};
use kernel::{HostHandler, Signal};
use kernel_apis::KernelApis;
use kernel_apis::goal::{GoalCheckResult, GoalDriver, GoalJobs, check_signature, goal_brief, goal_status_text};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn result_of(value: &Value) -> GoalCheckResult {
    GoalCheckResult {
        exit_code: value["exit_code"].as_i64(),
        timed_out: value["timed_out"] == true,
        output: value["output"].as_str().unwrap().to_string(),
    }
}

fn scripted(output: &str, exit_code: Option<i64>, timed_out: bool) -> GoalCheckResult {
    GoalCheckResult {
        exit_code,
        timed_out,
        output: output.into(),
    }
}

/// generate-goal.mjs CHECKS and its fake worker.
#[derive(Default)]
struct Worker {
    calls: Mutex<Vec<Value>>,
    used: Mutex<HashMap<String, usize>>,
    jobs: Mutex<u64>,
}

impl Worker {
    fn checks(command: &str) -> Vec<GoalCheckResult> {
        let fail = scripted("1 failed in 0.31s", Some(1), false);
        let pass = scripted("3 passed in 0.12s", Some(0), false);
        match command {
            "pytest -q" => vec![fail.clone(), fail, scripted("2 failed in 0.40s", Some(1), false), pass],
            "bench" => {
                let mut list = vec![scripted("slow: 812ms at 0x7f3a", Some(1), false); 10];
                list.push(scripted("slow: 640ms", Some(1), false));
                list
            }
            "long-output" => vec![scripted(&format!("{}é end", "y".repeat(2500)), Some(2), false)],
            "hang" => vec![scripted("", None, true)],
            _ => vec![pass],
        }
    }
}

impl GoalJobs for Worker {
    fn run_check(&self, command: &str, timeout_seconds: u64) -> Result<GoalCheckResult, String> {
        jsrt::sync::lock(&self.calls).push(json!(["check", command, timeout_seconds]));
        let list = Self::checks(command);
        let mut used = jsrt::sync::lock(&self.used);
        let count = used.entry(command.to_string()).or_default();
        let result = list[(*count).min(list.len() - 1)].clone();
        *count += 1;
        Ok(result)
    }

    fn start_job(&self, prompt: &str, key: &str, usage_root: &str) -> Result<String, String> {
        jsrt::sync::lock(&self.calls).push(json!(["start", key, usage_root, prompt]));
        if prompt.contains("unstartable") {
            return Err("no capacity for a background job".into());
        }
        let mut jobs = jsrt::sync::lock(&self.jobs);
        *jobs += 1;
        Ok(format!("task-{jobs}"))
    }

    fn stop_job(&self, task_id: &str, reason: &str) -> Result<(), String> {
        jsrt::sync::lock(&self.calls).push(json!(["stop", task_id, reason]));
        Ok(())
    }

    fn lane_of_job(&self, task_id: &str) -> Option<String> {
        Some(format!("ultron.background-job.{task_id}"))
    }
}

fn goal_host(_: &Path) -> Made {
    let store = Arc::new(MemoryDocumentStore::default());
    let worker = Arc::new(Worker::default());
    let driver = Arc::new(GoalDriver::new(store.clone(), worker.clone(), fixed(FIXED_NOW)));
    let test = driver.clone();
    let inner: Arc<dyn HostHandler> = Arc::new(move |kind: &str, payload: Map<String, Value>, _: &Signal| match kind {
        "test.goal.command" => Ok(json!({"text": test.command(payload["args"].as_str().unwrap())?})),
        "test.goal.ended" => {
            test.task_ended(payload["id"].as_str().unwrap(), payload.get("result"))?;
            Ok(Value::Null)
        }
        "test.goal.as" => test.handle_request(
            payload["type"].as_str().unwrap(),
            payload["payload"].as_object().unwrap(),
            payload["lane"].as_str().unwrap(),
        ),
        _ => Err(kernel::ops::not_wired(kind)),
    });
    Made {
        handler: KernelApis::new(inner).with(driver),
        env: Vec::new(),
        state: Box::new(move || json!({"document": store.read().unwrap(), "calls": *jsrt::sync::lock(&worker.calls)})),
    }
}

fn goal_ids(value: &Value) -> Value {
    let text = serde_json::to_string(value).unwrap();
    let pattern = fancy_regex::Regex::new("goal-[0-9a-f]{8}").unwrap();
    serde_json::from_str(&pattern.replace_all(&text, "goal-ID")).unwrap()
}

#[test]
fn kernel_apis_goal_cells_match_typescript_host() {
    let fixture = fixture("goal");
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let actual = goal_ids(&run_scenario(scenario, &goal_host));
        for (index, expected) in scenario["cells"].as_array().unwrap().iter().enumerate() {
            assert_eq!(actual["cells"][index], *expected, "{} / cell {index}", scenario["name"]);
        }
        assert_eq!(
            actual["state"]["calls"], scenario["state"]["calls"],
            "{} / calls",
            scenario["name"]
        );
        assert_eq!(
            actual["state"]["document"], scenario["state"]["document"],
            "{} / document",
            scenario["name"]
        );
        assert_eq!(actual, *scenario, "{}", scenario["name"]);
    }
}

#[test]
fn kernel_apis_goal_pure_functions_match_typescript() {
    let pure = &fixture("goal")["pure"];
    for case in pure["signatures"].as_array().unwrap() {
        assert_eq!(
            check_signature(&result_of(&case["result"])),
            case["signature"],
            "{case}"
        );
    }
    for case in pure["briefs"].as_array().unwrap() {
        assert_eq!(goal_brief(&case["goal"]), case["brief"], "{case}");
    }
    for case in pure["statuses"].as_array().unwrap() {
        let goal = Some(&case["goal"]).filter(|goal| !goal.is_null());
        assert_eq!(goal_status_text(goal), case["text"], "{case}");
    }
}

/// ultron-goal.test.ts "check signatures ignore durations and addresses, not counts or exit codes".
#[test]
fn kernel_apis_goal_check_signature_ignores_durations_and_addresses() {
    let fail = |output: &str| scripted(output, Some(1), false);
    assert_eq!(
        check_signature(&fail("1 failed in 0.31s at 0x7f00")),
        check_signature(&fail("1 failed in 12.5s at 0x1234"))
    );
    assert_ne!(check_signature(&fail("1 failed")), check_signature(&fail("2 failed")));
    assert_ne!(
        check_signature(&scripted("1 failed in 0.31s", Some(2), false)),
        check_signature(&fail("1 failed in 0.31s"))
    );
    let brief = goal_brief(&json!({"revision": 3, "objective": "o", "check": null, "last_answer": null}));
    assert!(brief.contains("There is no check command"));
}

/// ultron-goal.test.ts: a goal whose job was running loads paused after a restart; clear and status follow.
#[test]
fn kernel_apis_goal_restart_pauses_a_running_job() {
    let store = Arc::new(MemoryDocumentStore::default());
    let first = GoalDriver::new(store.clone(), Arc::new(Worker::default()), fixed(FIXED_NOW));
    first.command("ship the parser").unwrap();
    assert_eq!(first.get().unwrap().unwrap()["job"], "task-1");
    let again = GoalDriver::new(store, Arc::new(Worker::default()), fixed(FIXED_NOW));
    let goal = again.get().unwrap().unwrap();
    assert_eq!((goal["status"].as_str(), &goal["job"]), (Some("paused"), &Value::Null));
    assert_eq!(
        goal["reason"],
        "Ultron restarted while the goal job ran; /goal resume continues"
    );
    assert_eq!(again.command("clear").unwrap(), "Goal cleared.");
    assert_eq!(again.command("").unwrap(), "No goal. Set one with /goal <objective>.");
}

/// A worker whose job calls come back into the driver as the real task host does: stopping a running job ends it,
/// and its end reaches [`GoalDriver::task_ended`]; a job may also end before its start returns. Each call back runs
/// on a thread of its own and is waited for, so a driver that holds its lock across the call fails instead of
/// hanging the test.
#[derive(Default)]
struct Reentrant {
    driver: Mutex<std::sync::Weak<GoalDriver>>,
    /// The job id `start_job` returns, and whether that job ends before the start returns.
    ends_at_start: Mutex<bool>,
    jobs: Mutex<u64>,
    log: Mutex<Vec<Value>>,
}

impl Reentrant {
    fn back(&self, call: impl FnOnce(&GoalDriver) + Send + 'static) {
        let driver = jsrt::sync::lock(&self.driver).upgrade().unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            call(&driver);
            let _ = done.send(());
        });
        finished
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the driver took the call back while it made the job call");
    }
}

impl GoalJobs for Reentrant {
    fn run_check(&self, _: &str, _: u64) -> Result<GoalCheckResult, String> {
        // The person pauses the goal while its check runs.
        self.back(|driver| {
            driver.command("pause").unwrap();
        });
        Ok(scripted("1 failed", Some(1), false))
    }

    fn start_job(&self, _: &str, key: &str, _: &str) -> Result<String, String> {
        jsrt::sync::lock(&self.log).push(json!(["start", key]));
        let mut jobs = jsrt::sync::lock(&self.jobs);
        *jobs += 1;
        let job = format!("task-{jobs}");
        if *jsrt::sync::lock(&self.ends_at_start) {
            let ended = job.clone();
            self.back(move |driver| {
                driver
                    .task_ended(&ended, Some(&json!({"status": "failed", "error": "no model"})))
                    .unwrap();
            });
        }
        Ok(job)
    }

    fn stop_job(&self, task_id: &str, reason: &str) -> Result<(), String> {
        jsrt::sync::lock(&self.log).push(json!(["stop", task_id, reason]));
        // `Agents::cancel`'s cascade: it joins a thread that cancels the job's child, whose end reaches the driver
        // too (this happens even when the job was already ending on its own).
        let child = format!("{task_id}-child");
        self.back(move |driver| {
            driver
                .task_ended(
                    &child,
                    Some(&json!({"status": "cancelled", "error": "ancestor cancelled"})),
                )
                .unwrap();
        });
        let ended = task_id.to_string();
        self.back(move |driver| {
            driver
                .task_ended(&ended, Some(&json!({"status": "cancelled", "error": "stopped"})))
                .unwrap();
        });
        Ok(())
    }

    fn lane_of_job(&self, task_id: &str) -> Option<String> {
        Some(format!("ultron.background-job.{task_id}"))
    }
}

fn reentrant_driver(store: Arc<dyn DocumentStore>) -> (Arc<GoalDriver>, Arc<Reentrant>) {
    let jobs = Arc::new(Reentrant::default());
    let driver = Arc::new(GoalDriver::new(store, jobs.clone(), fixed(FIXED_NOW)));
    *jsrt::sync::lock(&jobs.driver) = Arc::downgrade(&driver);
    (driver, jobs)
}

/// B-report #1: pausing, replacing or clearing a goal whose job runs stops the job, and the job's end comes back
/// into the driver; the driver must not hold its lock across that (it deadlocked on its own mutex). Also the second
/// path the goal-driver spec found (NoWaitCycle): the cancel cascade joins a thread whose child-task end waits for
/// the driver's lock, so the stop must not be made under it either.
#[test]
fn kernel_apis_goal_pause_replace_and_clear_stop_a_job_whose_end_calls_back() {
    let (driver, jobs) = reentrant_driver(Arc::new(MemoryDocumentStore::default()));
    driver.command("ship it").unwrap();
    assert_eq!(driver.get().unwrap().unwrap()["job"], "task-1");
    let paused = driver.command("pause").unwrap();
    assert!(paused.contains("paused"), "{paused}");
    let goal = driver.get().unwrap().unwrap();
    assert_eq!(
        (goal["status"].clone(), goal["job"].clone()),
        (json!("paused"), Value::Null)
    );

    driver.command("resume").unwrap();
    assert_eq!(driver.get().unwrap().unwrap()["job"], "task-2");
    driver.command("ship it faster").unwrap();
    let goal = driver.get().unwrap().unwrap();
    assert_eq!(
        (goal["revision"].clone(), goal["job"].clone()),
        (json!(2), json!("task-3"))
    );
    assert_eq!(driver.command("clear").unwrap(), "Goal cleared.");
    assert_eq!(driver.get().unwrap(), None);
    let log: Vec<Value> = jsrt::sync::lock(&jobs.log).iter().map(|e| e[0].clone()).collect();
    assert_eq!(log, ["start", "stop", "start", "stop", "start", "stop"]);
}

/// A job that ends before its start returns (its end reaches the driver first) is settled as its goal's job: the
/// goal pauses (no progress) instead of waiting for an end that already happened.
#[test]
fn kernel_apis_goal_job_that_ends_before_its_start_returns_is_settled() {
    let (driver, jobs) = reentrant_driver(Arc::new(MemoryDocumentStore::default()));
    *jsrt::sync::lock(&jobs.ends_at_start) = true;
    driver.command("ship it").unwrap();
    let goal = driver.get().unwrap().unwrap();
    assert_eq!(
        (goal["status"].clone(), goal["job"].clone(), goal["last_answer"].clone()),
        (json!("paused"), Value::Null, json!("no model"))
    );
}

/// The check runs with the lock released (a person's /goal is not held up for the check's run), and its result is
/// recorded on the goal it ran for.
#[test]
fn kernel_apis_goal_check_runs_without_holding_the_goal() {
    let (driver, _jobs) = reentrant_driver(Arc::new(MemoryDocumentStore::default()));
    driver.command("ship it").unwrap();
    driver.command("check pytest -q").unwrap();
    let run = driver
        .handle_request("goal.check", &Map::new(), kernel::ROOT_LANE)
        .unwrap();
    assert_eq!(
        (run["passed"].clone(), run["paused"].clone()),
        (json!(false), json!(false))
    );
    let goal = driver.get().unwrap().unwrap();
    assert_eq!(
        (goal["status"].clone(), goal["checks"].clone()),
        (json!("paused"), json!(1))
    );
}

/// A goal document that cannot be written is an error of the command, and a run that could not be written is not
/// started (its key would not be found after a restart).
#[test]
fn kernel_apis_goal_write_errors_are_reported() {
    struct Broken;
    impl DocumentStore for Broken {
        fn read(&self) -> Result<Option<Value>, String> {
            Ok(None)
        }
        fn write(&self, _: &Value) -> Result<(), String> {
            Err("disk full".into())
        }
    }
    let (driver, jobs) = reentrant_driver(Arc::new(Broken));
    let error = driver.command("ship it").unwrap_err();
    assert!(error.contains("disk full"), "{error}");
    assert!(jsrt::sync::lock(&jobs.log).is_empty());
    assert_eq!(driver.get().unwrap().unwrap()["status"], "paused");
}
