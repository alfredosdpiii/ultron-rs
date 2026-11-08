//! `review_api.post` (`/review --post`), run in a kernel cell with a fake `git` and `gh`: the comment it posts is the
//! report without the local path or the "post pending" line, and a report is posted once.

use kernel::{HostHandler, Kernel, KernelOptions, Signal, Status};
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Arc;

#[test]
fn review_post_sends_the_final_report_once() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py");
    let handler: Arc<dyn HostHandler> = Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null));
    let kernel = Kernel::new(KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime), handler).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let code = [
        "import json, review_api as r",
        &format!("d = {:?}", directory.path().display().to_string()),
        "git = r.Git(d, lambda argv, cwd, timeout: (0, d + '\\n', ''))",
        "args = dict(label='main', files=[], reviewers=[], confirmed=[], uncertain=[], rejected=[], stats={},",
        "            not_checked=[], read_file=lambda name: None)",
        "review = r._finish(args, git, 7, True)",
        "bodies = []",
        "def gh(argv, cwd, timeout):",
        "    bodies.append(open(argv[-1]).read())",
        "    return (0, '', '')",
        "await r.post(review, confirm=True, runner=gh, which=lambda name: '/usr/bin/gh')",
        "try:",
        "    await r.post(review.path, confirm=True, pr=7, runner=gh, which=lambda name: '/usr/bin/gh')",
        "    second = 'posted again'",
        "except r.ReviewError as error:",
        "    second = str(error)",
        "print(json.dumps({'report': review.report, 'path': review.path, 'bodies': bodies, 'second': second,",
        "                  'pending': review.post_pending}))",
    ]
    .join("\n");
    let result = kernel.execute(&code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let out: Value = serde_json::from_str(result.stdout.trim()).unwrap();
    let path = out["path"].as_str().unwrap();
    let report = out["report"].as_str().unwrap();
    assert!(report.contains(&format!("- Full report: {path}")), "{report}");
    assert!(
        report.contains("- Post pending: nothing has been posted to PR #7."),
        "{report}"
    );
    let bodies = out["bodies"].as_array().unwrap();
    assert_eq!(bodies.len(), 1, "posted once: {out}");
    let body = bodies[0].as_str().unwrap();
    assert!(body.starts_with("# Code review: main\n"), "{body}");
    assert!(!body.contains(path) && !body.contains("Full report"), "{body}");
    assert!(!body.contains("Post pending"), "{body}");
    assert_eq!(out["second"], "this report was already posted to PR #7");
    assert_eq!(out["pending"], false);
}
