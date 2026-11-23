//! Tests for REPL polish: AST pre-flight host-binding protection, bounded queryable `console` buffer,
//! and sticky fail-closed `files.before_write` enforcement at the host boundary.

use kernel::{ExecutionResult, HostHandler, Kernel, KernelOptions, Signal, Status};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn kernel_in(cwd: &Path, handler: Arc<dyn HostHandler>) -> Kernel {
    Kernel::new(KernelOptions::new(cwd, runtime()), handler).expect("kernel options")
}

fn null_handler() -> Arc<dyn HostHandler> {
    Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null))
}

fn run(kernel: &Kernel, code: &str) -> ExecutionResult {
    kernel
        .execute(code, None, None)
        .unwrap_or_else(|error| panic!("{code}: {error}"))
}

#[test]
fn kernel_ast_preflight_blocks_overwriting_protected_host_bindings_without_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel_in(dir.path(), null_handler());

    // Blocked forms: assignment, augmented assignment, del, def, async def, class, import, for, with, except,
    // walrus (including inside comprehension), and `global <protected>` mutated inside a function.
    let blocked = [
        "side_effect = 1\nbash = 123",
        "del edit",
        "def write(): pass",
        "async def read(): pass",
        "class rlm: pass",
        "import math as preview",
        "from math import sin as console",
        "for mcp in [1]: pass",
        "with open('/dev/null') as tools: pass",
        "try:\n    1/0\nexcept ZeroDivisionError as ctx:\n    pass",
        "[x for x in [1] if (bash := x)]",
        "def sneak():\n    global bash\n    bash = lambda *a: None",
    ];

    for code in blocked {
        let res = run(&kernel, code);
        assert_eq!(res.status, Status::Error, "{code}: expected ReplGlobalOverwrite");
        let err = res.error.as_ref().unwrap();
        assert_eq!(err.ename, "ReplGlobalOverwrite", "{code}: {err:?}");
        assert!(
            err.evalue.contains("cannot overwrite protected host binding"),
            "{code}: {err:?}"
        );
    }

    // Because AST pre-flight ran before execution, `side_effect` from the first blocked cell was never bound.
    let check = run(
        &kernel,
        "'side_effect' in globals() and callable(bash) and callable(write)",
    );
    assert_eq!(check.status, Status::Ok);
    assert_eq!(check.result.as_deref(), Some("False"));

    // Allowed forms: local variables/params in functions, class methods, comprehension targets, stdlib re-imports.
    let allowed = run(
        &kernel,
        concat!(
            "import os, json, re, asyncio\n",
            "from pathlib import Path\n",
            "def f(read, edit=1):\n",
            "    bash = read + edit\n",
            "    return bash\n",
            "class C:\n",
            "    def write(self): return 7\n",
            "vals = [bash for bash in range(3)]\n",
            "(f(2), C().write(), vals, callable(bash))"
        ),
    );
    assert_eq!(allowed.status, Status::Ok, "{allowed:?}");
    assert_eq!(allowed.result.as_deref(), Some("(3, 7, [0, 1, 2], True)"));
}

#[test]
fn kernel_console_buffer_separates_quiet_logs_and_capture_from_model_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel_in(dir.path(), null_handler());

    // Normal print goes to model-facing stdout AND is retained in `console`.
    let c1 = run(
        &kernel,
        "print('line-1')\nprint('err-1', file=__import__('sys').stderr)",
    );
    assert_eq!(c1.status, Status::Ok);
    assert_eq!(c1.stdout, "line-1\n");
    assert_eq!(c1.stderr, "err-1\n");

    // `console.log` and `with console.capture():` stay out of model-facing stdout/stderr.
    let c2 = run(
        &kernel,
        concat!(
            "console.log('quiet-log-1')\n",
            "with console.capture():\n",
            "    print('captured-out-1')\n",
            "    print('captured-err-1', file=__import__('sys').stderr)\n",
            "{'ok': True}"
        ),
    );
    assert_eq!(c2.status, Status::Ok, "{c2:?}");
    assert_eq!(c2.stdout, "");
    assert_eq!(c2.stderr, "");
    assert_eq!(c2.result.as_deref(), Some("{'ok': True}"));

    // Later turns can query `console` selectively with tail, head, search, and preview.
    let c3 = run(
        &kernel,
        concat!(
            "print(json.dumps({\n",
            "    'all_tail': console.tail(3),\n",
            "    'log_tail': console.tail(5, stream='log'),\n",
            "    'err_tail': console.tail(5, stream='stderr'),\n",
            "    'out_head': console.head(2, stream='stdout'),\n",
            "    'search': console.search(r'captured-.*-1'),\n",
            "    'preview_lines': console.preview(max_bytes=128)['lines'],\n",
            "}))"
        ),
    );
    assert_eq!(c3.status, Status::Ok, "{c3:?}");
    let parsed: Value = serde_json::from_str(c3.stdout.trim()).unwrap();
    assert_eq!(parsed["all_tail"], json!("quiet-log-1\ncaptured-out-1\ncaptured-err-1"));
    assert_eq!(parsed["log_tail"], json!("quiet-log-1"));
    assert_eq!(parsed["err_tail"], json!("err-1\ncaptured-err-1"));
    assert_eq!(parsed["out_head"], json!("line-1\ncaptured-out-1"));
    assert_eq!(parsed["search"], json!(["captured-out-1", "captured-err-1"]));
    assert_eq!(parsed["preview_lines"], json!(5));

    // Bounding: overflowing `console` evicts oldest entries and tracks dropped counts.
    let c5 = run(
        &kernel,
        concat!(
            "console.clear()\n",
            "for i in range(4200):\n",
            "    console.log(f'row-{i}')\n",
            "print(json.dumps({'stats': console.stats(), 'tail': console.tail(2)}))"
        ),
    );
    assert_eq!(c5.status, Status::Ok, "{c5:?}");
    let bounded: Value = serde_json::from_str(c5.stdout.trim()).unwrap();
    assert!(bounded["stats"]["dropped_lines"].as_u64().unwrap() >= 200);
    assert_eq!(bounded["tail"], json!("row-4198\nrow-4199"));
}

#[test]
fn kernel_file_hooks_stay_sticky_and_fail_closed_on_host_error() {
    let dir = tempfile::tempdir().unwrap();
    let handler: Arc<dyn HostHandler> = Arc::new(|kind: &str, payload: Map<String, Value>, _: &Signal| {
        if kind != "files.before_write" {
            return Err(format!("unexpected {kind}"));
        }
        let content = payload["writes"][0]["content"].as_str().unwrap_or("");
        if content == "TRIGGER_HOST_ERR" {
            return Err("policy service unavailable".into());
        }
        Ok(json!({ "results": [{ "blocked": content == "BLOCKED", "reason": "host denied" }] }))
    });
    let mut opts = KernelOptions::new(dir.path(), runtime());
    opts.env.push(("ULTRON_FILE_HOOKS".into(), "1".into()));
    let kernel = Kernel::new(opts, handler).expect("kernel options");

    // Mutating os.environ cannot disable file hooks once enabled.
    let bypass = run(
        &kernel,
        "os.environ['ULTRON_FILE_HOOKS'] = '0'\nawait write('secret.txt', 'BLOCKED')",
    );
    assert_eq!(bypass.status, Status::Error, "{bypass:?}");
    let err = bypass.error.unwrap();
    assert_eq!(err.ename, "ValueError");
    assert!(err.evalue.contains("host denied"), "{err:?}");
    assert!(!dir.path().join("secret.txt").exists());

    // A host error on files.before_write fails closed instead of writing unchecked.
    let fail_closed = run(&kernel, "await write('fail.txt', 'TRIGGER_HOST_ERR')");
    assert_eq!(fail_closed.status, Status::Error, "{fail_closed:?}");
    let err = fail_closed.error.unwrap();
    assert_eq!(err.ename, "ValueError");
    assert!(err.evalue.contains("write hook check failed"), "{err:?}");
    assert!(!dir.path().join("fail.txt").exists());

    // Clean write still succeeds.
    let ok = run(&kernel, "await write('ok.txt', 'hello')");
    assert_eq!(ok.status, Status::Ok, "{ok:?}");
    assert_eq!(std::fs::read_to_string(dir.path().join("ok.txt")).unwrap(), "hello");
}
