//! A05 (partial): cancellation terminates owned work and rejects late success.
//! Ported from packages/coding-agent/test/ultron-a05-cancellation-process-tree.test.ts: the two cell cases, over the
//! real runtime.py under the Rust kernel supervisor. The three task cases ran over a TS-era task supervisor the
//! product does not use; the product task host's cancellation and late-success rules are `a05_native_host.rs`.

use crate::common;

use common::{alive, kernel_in, null_handler, read_pids, run, wait_until};
use kernel::{HostHandler, KernelError, Signal, Status};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

/// Python cell: a child with a same-group grandchild and a setsid grandchild, the pids written to `pid_file`, then
/// `wait`.
fn process_tree_cell(pid_file: &Path, wait: &str) -> String {
    let path = serde_json::to_string(pid_file).unwrap();
    [
        "import json, os, subprocess, sys",
        "child_code = '''",
        "import subprocess, sys, time",
        "g1 = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'])",
        "g2 = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'], start_new_session=True)",
        "print(g1.pid, g2.pid, flush=True)",
        "time.sleep(120)",
        "'''",
        "proc = subprocess.Popen([sys.executable, '-c', child_code], stdout=subprocess.PIPE, text=True)",
        "grandchild, detached = map(int, proc.stdout.readline().split())",
        &format!(
            "open({path}, 'w').write(json.dumps({{'kernel': os.getpid(), 'child': proc.pid, 'grandchild': grandchild, 'detached': detached}}))"
        ),
        wait,
        "'finished'",
    ]
    .join("\n")
}

fn all_gone(pids: &[u32]) {
    wait_until("the process tree to end", || pids.iter().all(|pid| !alive(*pid)));
}

#[derive(Default)]
struct Gate(Mutex<bool>, Condvar);

impl Gate {
    fn open(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
    fn wait(&self) {
        drop(self.1.wait_while(self.0.lock().unwrap(), |open| !*open).unwrap());
    }
}

fn cancelling_a_cell_kills_its_tree_and_a_late_reply_does_not_revive_it() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pids.json");
    let gate = Arc::new(Gate::default());
    let (signals, host_signal) = mpsc::channel::<Signal>();
    let (answered, late_answer) = mpsc::channel::<()>();
    let late = gate.clone();
    let (signals, answered) = (Mutex::new(signals), Mutex::new(answered));
    // Delayed provider: answers only after the test opens the gate, ignoring the abort signal.
    let handler: Arc<dyn HostHandler> = Arc::new(move |_: &str, _: Map<String, Value>, signal: &Signal| {
        let _ = signals.lock().unwrap().send(signal.clone());
        late.wait();
        let _ = answered.lock().unwrap().send(());
        Ok(json!({"late": "success"}))
    });
    let kernel = Arc::new(kernel_in(dir.path(), handler));
    let signal = Signal::new();
    let (cell_kernel, cell_signal) = (kernel.clone(), signal.clone());
    let code = process_tree_cell(&pid_file, "late = await rlm.host_request('provider.call', {})");
    let running = std::thread::spawn(move || cell_kernel.execute(&code, Some(&cell_signal), None));
    let pids = read_pids(&pid_file, 4);
    let host_signal = host_signal.recv_timeout(common::HANG).unwrap();
    assert!(pids.iter().all(|pid| alive(*pid)));

    signal.abort("user cancelled");
    assert_eq!(running.join().unwrap(), Err(KernelError::new("user cancelled")));
    assert!(host_signal.is_aborted());
    all_gone(&pids);

    // The late provider success arrives after cancellation and must not revive the cell.
    gate.open();
    late_answer.recv_timeout(common::HANG).unwrap();
    // A short settle for the answer to reach the kernel; a slower machine can only make this pass without testing.
    std::thread::sleep(Duration::from_millis(50));
    let fresh = run(&kernel, "'late' in globals()");
    assert_eq!((fresh.status, fresh.result.as_deref()), (Status::Ok, Some("False")));
    kernel.shutdown();
}

fn shutdown_kills_the_tree_of_a_cell_blocked_in_synchronous_code() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pids.json");
    let kernel = Arc::new(kernel_in(dir.path(), null_handler()));
    let cell_kernel = kernel.clone();
    let code = process_tree_cell(&pid_file, "import time\ntime.sleep(120)");
    let running = std::thread::spawn(move || cell_kernel.execute(&code, None, None));
    let pids = read_pids(&pid_file, 4);
    kernel.shutdown();
    let error = running.join().unwrap().unwrap_err();
    assert!(error.message().contains("shut down"), "{error}");
    all_gone(&pids);
}

#[test]
fn acceptance_a05_cancellation_kills_owned_work_and_rejects_late_success_ported_part() {
    cancelling_a_cell_kills_its_tree_and_a_late_reply_does_not_revive_it();
    shutdown_kills_the_tree_of_a_cell_blocked_in_synchronous_code();
}
