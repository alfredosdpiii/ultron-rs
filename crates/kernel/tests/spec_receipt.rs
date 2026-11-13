//! The spec of a change (`spec_api`), run in a kernel cell on a real repository: what the finished work must do is
//! written before the work, each item ends with evidence, checks are recorded with their exit code for the tree
//! they ran on, and the pull request's description is built from those and the review's receipt, or refused.

use kernel::{HostHandler, Kernel, KernelOptions, Signal, Status};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::Arc;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn a_description_is_built_from_the_spec_the_checks_and_the_review_or_refused() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py");
    let handler: Arc<dyn HostHandler> = Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null));
    let kernel = Kernel::new(KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime), handler).unwrap();
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("a.py"), "one\n").unwrap();
    git(dir, &["add", "a.py"]);
    git(dir, &["commit", "-qm", "one"]);
    let tree = git(dir, &["rev-parse", "HEAD^{tree}"]);
    let code = [
        "import json, os, review_api as r, spec_api as s",
        &format!("d = {:?}", dir.display().to_string()),
        "receipt = os.path.join(d, '.git', 'ultron-review', 'receipt.json')",
        "def error(call):",
        "    try:",
        "        return call()",
        "    except r.ReviewError as e:",
        "        return 'error: ' + str(e)",
        "class Ran(str):",
        "    pass",
        "def shell(exit_code):",
        "    async def bash(command, yield_after=None):",
        "        ran = Ran('collected 3 items\\n3 passed in 0.1s\\n\\n')",
        "        ran.exit_code, ran.timed_out = exit_code, False",
        "        return ran",
        "    return bash",
        "def describe():",
        "    return error(lambda: open(s.description(cwd=d)).read())",
        "def reviewed(**fields):",
        "    body = {'tree': r.Git(d).value('rev-parse', 'HEAD^{tree}'), 'confirmed': 0, 'incomplete': False,",
        "            'waived': None, 'files': ['a.py'], 'purposes': [{'purpose': 'add daily totals', 'files': ['a.py']}]}",
        "    json.dump({**body, **fields}, open(receipt, 'w'))",
        "out = {'unreviewed': describe()}",
        "out['empty'] = error(lambda: s.write('Totals', 'Totals per day.', [' '], cwd=d))",
        "out['written'] = s.write('Totals', 'The page shows totals per day.',",
        "                         ['A day with sales shows their sum', 'A day without sales shows 0'], cwd=d)",
        "out['again'] = error(lambda: s.write('Other', 'x', ['y'], cwd=d))",
        "try:",
        "    await s.check(shell(0), 'pytest -q | tail -5', cwd=d)",
        "except r.ReviewError as e:",
        "    out['piped'] = str(e)",
        "out['quoted_pipe'] = (await s.check(shell(0), \"rg -c 'a|b' a.py\", cwd=d))['exit_code']",
        "out['no_evidence'] = error(lambda: s.mark('A1', 'pass', ' ', cwd=d))",
        "out['unknown'] = error(lambda: s.mark('A9', 'pass', 'x', cwd=d))",
        "s.mark('A1', 'pass', 'pytest -q tests/test_totals.py: 3 passed', cwd=d)",
        // Reviewed, with one item still pending: refused, naming it.
        "await r.run(rlm, '--model fake/model', cwd=d)",
        "out['clean_receipt'] = json.load(open(receipt))",
        "reviewed()",
        "out['pending'] = describe()",
        "s.mark('A2', 'blocked', 'no browser on this machine', cwd=d)",
        "out['blocked'] = describe()",
        "s.mark('A2', 'pass', 'curl /totals?day=2026-01-01: 0', cwd=d)",
        "out['passed'] = describe()",
        // A failing run of the same command is the latest word on it.
        "await s.check(shell(2), \"rg -c 'a|b' a.py\", cwd=d)",
        "out['failing'] = describe()",
        "await s.check(shell(0), \"rg -c 'a|b' a.py\", cwd=d)",
        // Two purposes and a file no purpose names: refused until waived with a reason, which the description prints.
        "reviewed(files=['a.py', 'b.py'], purposes=[{'purpose': 'add daily totals', 'files': ['a.py']},",
        "                                           {'purpose': 'reformat the audit module', 'files': []}])",
        "out['mixed'] = describe()",
        "r.waive('the reformat is one line the totals need', cwd=d)",
        "out['waived'] = describe()",
        "reviewed(confirmed=1)",
        "out['finding'] = describe()",
        // The spec grew: said so, with the items that stay keeping their result.
        "reviewed()",
        "s.write('Totals', 'The page shows totals per day.', ['A day with sales shows their sum', 'Totals are in UTC'],",
        "        cwd=d, revise='the ticket added time zones')",
        "out['revised'] = json.load(open(os.path.join(d, '.git', 'ultron-review', 'spec.json')))",
        // A check on an uncommitted change says nothing about the committed tree.
        "open(os.path.join(d, 'a.py'), 'w').write('two\\n')",
        "out['dirty_check'] = (await s.check(shell(0), 'pytest -q', cwd=d))['dirty']",
        // `/review` holds the diff against the items: a finder of their own, planned with the others.
        "items = r._acceptance_items(r.Git(d))",
        "out['items'] = items",
        "scope = r.resolve_scope(r.parse_args('', {}), d)",
        "chunks, _ = r.build_chunks(scope.files, scope.read_file)",
        "plan = r.plan_find(chunks, [*r.parse_args('', {}).reviewers, r.acceptance_reviewer(items)], 100000)",
        "out['frames'] = [reviewer.key for reviewer, _ in plan['frames']]",
        "out['task'] = r.finder_task(r.acceptance_reviewer(items))",
        "print(json.dumps(out))",
    ]
    .join("\n");
    let result = kernel.execute(&code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let out: Value = serde_json::from_str(result.stdout.trim()).unwrap();
    let text = |key: &str| out[key].as_str().unwrap_or_else(|| panic!("{key}: {out}")).to_string();

    assert!(text("unreviewed").contains("has not been reviewed"), "{out}");
    assert!(text("empty").starts_with("error: "), "{out}");
    assert!(text("written").contains("A1 A day with sales shows their sum"), "{out}");
    assert!(
        text("again").contains("revise="),
        "a spec is not replaced silently: {out}"
    );
    assert!(text("piped").contains("hides its exit code"), "{out}");
    assert_eq!(out["quoted_pipe"], 0, "a | inside quotes is no pipe: {out}");
    assert!(text("no_evidence").contains("evidence"), "{out}");
    assert!(text("unknown").contains("A9"), "{out}");
    assert_eq!(out["clean_receipt"]["files"], json!([]), "{out}");
    assert_eq!(out["clean_receipt"]["purposes"], json!([]), "{out}");

    assert!(text("pending").contains("A2 is pending"), "{out}");
    assert!(
        text("blocked").contains("A2 is blocked"),
        "blocked is not a pass: {out}"
    );
    let passed = text("passed");
    assert!(
        passed.starts_with(&format!("<!-- ultron-description tree={tree} -->\n")),
        "{passed}"
    );
    assert!(
        passed.contains("## Why\n\nThe page shows totals per day.\n"),
        "{passed}"
    );
    assert!(
        passed.contains("## What changed\n\n- add daily totals (`a.py`)\n"),
        "{passed}"
    );
    assert!(
        passed.contains("- pass A1 A day with sales shows their sum: pytest -q tests/test_totals.py: 3 passed\n"),
        "{passed}"
    );
    assert!(
        passed.contains("## Verification\n\n- `rg -c 'a|b' a.py`: exit 0 on this tree, last line `3 passed in 0.1s`\n"),
        "{passed}"
    );
    assert!(!passed.contains("## Scope"), "nothing open: {passed}");
    assert!(text("failing").contains("exit 2"), "{out}");

    let mixed = text("mixed");
    assert!(mixed.contains("2 separate purposes"), "{mixed}");
    assert!(mixed.contains("b.py"), "a file no purpose names: {mixed}");
    assert!(
        mixed.contains("review_api.waive"),
        "how to go on without the user: {mixed}"
    );
    let waived = text("waived");
    assert!(
        waived.contains("## Scope\n\n- Ships with open items (the reformat is one line the totals need):"),
        "{waived}"
    );
    assert!(waived.contains("2 separate purposes"), "{waived}");
    assert!(text("finding").contains("1 confirmed finding"), "{out}");

    let revised = &out["revised"];
    assert_eq!(revised["revisions"], json!(["the ticket added time zones"]));
    assert_eq!(
        revised["acceptance"][0]["status"], "pass",
        "a kept item keeps its result: {revised}"
    );
    assert_eq!(
        revised["acceptance"][1],
        json!({"id": "A2", "text": "Totals are in UTC", "status": "pending", "evidence": ""})
    );
    assert_eq!(out["dirty_check"], true);
    assert_eq!(
        out["items"],
        json!([
            "A1 (pass): A day with sales shows their sum",
            "A2 (pending): Totals are in UTC"
        ])
    );
    assert_eq!(
        out["frames"].as_array().unwrap().last(),
        Some(&json!("acceptance")),
        "{out}"
    );
    assert!(text("task").contains("A2 (pending): Totals are in UTC"), "{out}");
}
