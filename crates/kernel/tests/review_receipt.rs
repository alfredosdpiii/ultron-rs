//! The ship receipt of `/review` (`review_api.run`, `review_api.waive`), run in a kernel cell on a real repository:
//! a review of the whole committed change names the tree it reviewed, and a review with uncommitted tracked changes
//! writes none, since a pull request would not carry them.

use kernel::{HostHandler, Kernel, KernelOptions, Signal, Status};
use serde_json::{Map, Value};
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
fn review_writes_a_receipt_for_the_committed_tree_only() {
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
        "import json, os, review_api as r",
        &format!("d = {:?}", dir.display().to_string()),
        "receipt = os.path.join(d, '.git', 'ultron-review', 'receipt.json')",
        "def read():",
        "    return json.load(open(receipt)) if os.path.exists(receipt) else None",
        "def error(call):",
        "    try:",
        "        return call()",
        "    except r.ReviewError as e:",
        "        return 'error: ' + str(e)",
        "out = {'no_review': error(lambda: r.waive('why', cwd=d))}",
        // Nothing to review on a clean tree: no model is called, and the tree counts as reviewed.
        "clean = await r.run(rlm, '--model fake/model', cwd=d)",
        "out['clean_report'] = clean.report",
        "out['clean'] = read()",
        "out['empty_reason'] = error(lambda: r.waive('  ', cwd=d))",
        "out['waived'] = r.waive('a follow-up', cwd=d)",
        "out['after_waive'] = read()",
        "os.remove(receipt)",
        "partial = await r.run(rlm, '--model fake/model --only bugs', cwd=d)",
        "out['only_bugs'] = read()",
        "open(os.path.join(d, 'a.py'), 'w').write('two\\n')",
        "scope = r.resolve_scope(r.parse_args('', {}), d)",
        "out['dirty'] = r._write_receipt(r.Git(d), scope, r.parse_args('', {}), 0, False)",
        "out['dirty_receipt'] = read()",
        "class Fake:",
        "    async def infer(self, task, context, **kwargs):",
        "        return ['add Okta login', 'reformat the audit module']",
        "class Broken:",
        "    async def infer(self, task, context, **kwargs):",
        "        raise RuntimeError('no model')",
        "out['purposes'] = await r._purposes(Fake(), [], [], None)",
        "out['no_purposes'] = await r._purposes(Broken(), [], [], None)",
        "args = dict(label='main', files=[], reviewers=[], confirmed=[], uncertain=[], rejected=[], stats={},",
        "            not_checked=[], read_file=lambda name: None)",
        "out['report'] = r.render_report(**args, purposes=out['purposes'])",
        "class Bridge:",
        "    async def request(self, kind, payload):",
        "        if payload['scope'] == 'project':",
        "            raise RuntimeError('memory is off')",
        "        return {'results': [{'text': 'Review lesson: a failed cache clear skipped the token delete.'},",
        "                            {'text': 'The user likes short answers.'}, 'junk']}",
        "class WithMemory:",
        "    _bridge = Bridge()",
        "lessons = await r._lessons(WithMemory())",
        "out['lessons'] = lessons",
        "out['no_memory'] = await r._lessons(object())",
        "sixth = r.lessons_reviewer(lessons)",
        "chunks, _ = r.build_chunks(scope.files, scope.read_file)",
        "plan = r.plan_find(chunks, [*r.parse_args('', {}).reviewers, sixth], 100000)",
        "out['frames'] = [reviewer.key for reviewer, _ in plan['frames']]",
        "out['task'] = r.finder_task(sixth)",
        "print(json.dumps(out))",
    ]
    .join("\n");
    let result = kernel.execute(&code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let out: Value = serde_json::from_str(result.stdout.trim()).unwrap();

    assert_eq!(
        out["no_review"],
        "error: no review of the committed tree to waive: run the review first"
    );
    assert_eq!(out["clean"]["tree"], tree.as_str(), "{out}");
    assert_eq!(out["clean"]["confirmed"], 0);
    assert_eq!(out["clean"]["incomplete"], false);
    assert_eq!(out["clean"]["waived"], Value::Null);
    let report = out["clean_report"].as_str().unwrap();
    assert!(
        report.contains(&format!("Ship receipt written for tree {}.", &tree[..8])),
        "{report}"
    );
    assert_eq!(
        out["empty_reason"],
        "error: waive needs the reason the findings may ship"
    );
    assert_eq!(out["after_waive"]["waived"], "a follow-up");
    assert_eq!(out["after_waive"]["tree"], tree.as_str());
    assert_eq!(
        out["only_bugs"],
        Value::Null,
        "a review by some reviewers is no receipt: {out}"
    );
    assert_eq!(
        out["dirty"],
        "No ship receipt: tracked files have uncommitted changes. Commit, then review again."
    );
    assert_eq!(out["dirty_receipt"], Value::Null, "{out}");

    // Lessons kept in memory get a finder of their own; anything else recalled, and a failing scope, are left out.
    let lesson = "Review lesson: a failed cache clear skipped the token delete.";
    assert_eq!(out["lessons"], serde_json::json!([lesson]), "{out}");
    assert_eq!(out["no_memory"], serde_json::json!([]));
    assert_eq!(out["frames"].as_array().unwrap().last().unwrap(), "lessons", "{out}");
    assert!(out["task"].as_str().unwrap().contains(&format!("- {lesson}")), "{out}");

    assert_eq!(
        out["no_purposes"],
        serde_json::json!([]),
        "a failed frame leaves the review standing"
    );
    let report = out["report"].as_str().unwrap();
    assert!(
        report.contains("## Purposes of this change (2)\n\n- add Okta login\n- reformat the audit module\n"),
        "{report}"
    );
    assert!(report.contains("must name each one"), "{report}");
}

#[test]
fn a_review_lesson_is_kept_once() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py");
    let handler: Arc<dyn HostHandler> = Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null));
    let kernel = Kernel::new(KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime), handler).unwrap();
    let code = [
        "import json, review_api as r",
        "class Memory:",
        "    def __init__(self, kept):",
        "        self.kept, self.proposed = kept, []",
        "    async def prepare(self, query, *, scope):",
        "        return {'results': [{'memoryId': f'm{n}', 'text': text} for n, text in enumerate(self.kept)]}",
        "    async def propose(self, text, evidence, **fields):",
        "        self.proposed.append([text, fields['scope'], fields['key']])",
        "        return {'id': 'new'}",
        "class Judge:",
        "    def __init__(self, same, settled=True):",
        "        self.same, self.settled, self.asked = same, settled, []",
        "    async def decide(self, state, questions, kind=None):",
        "        self.asked.append([state, kind, len(questions)])",
        "        answers = {name: {'value': self.same in q['instructions']} for name, q in questions.items()}",
        "        return {'settled': self.settled, 'answers': answers, 'rung': 'first' if self.settled else 'review'}",
        "kept = ['Review lesson: errors were mapped to success.', 'The user likes short answers.',",
        "        'Review lesson: tests pass with the guard removed.']",
        "new = 'Review lesson: a  test still passes when the guard is deleted.'",
        "ref = [{'ref': 'https://example.invalid/pull/1'}]",
        "out = {}",
        "memory, judge = Memory(kept), Judge('guard removed')",
        "out['duplicate'] = await r.keep_lesson(memory, judge, new, ref, key='review-lesson-tests')",
        "out['duplicate_proposed'], out['asked'] = memory.proposed, judge.asked",
        "memory = Memory(kept)",
        "out['new'] = await r.keep_lesson(memory, Judge('nothing alike'), new, ref, key='review-lesson-tests')",
        "out['new_proposed'] = memory.proposed",
        "memory = Memory(kept)",
        "out['unsure'] = await r.keep_lesson(memory, Judge('guard removed', settled=False), new, ref)",
        "out['unsure_proposed'] = memory.proposed",
        "memory, judge = Memory([]), Judge('x')",
        "out['first'] = await r.keep_lesson(memory, judge, new, ref)",
        "out['first_asked'] = judge.asked",
        "try:",
        "    await r.keep_lesson(memory, judge, 'Tests are good.', ref)",
        "except r.ReviewError as e:",
        "    out['unmarked'] = str(e)",
        "print(json.dumps(out))",
    ]
    .join("\n");
    let result = kernel.execute(&code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let out: Value = serde_json::from_str(result.stdout.trim()).unwrap();
    let new = "Review lesson: a test still passes when the guard is deleted.";

    // The judge is asked once, about the kept lessons only, and its yes names the one already kept.
    assert_eq!(
        out["duplicate"],
        serde_json::json!({"kept": false, "duplicate_of": {"id": "m2", "text": "Review lesson: tests pass with the guard removed."}})
    );
    assert_eq!(out["duplicate_proposed"], serde_json::json!([]));
    assert_eq!(
        out["asked"],
        serde_json::json!([[format!("Candidate: {new}"), "review.lesson", 2]])
    );
    assert_eq!(out["new"], serde_json::json!({"kept": true, "id": "new"}));
    assert_eq!(
        out["new_proposed"],
        serde_json::json!([[new, "global", "review-lesson-tests"]])
    );
    // A judge that did not settle it writes nothing.
    assert_eq!(out["unsure"], serde_json::json!({"kept": false, "unsure": "review"}));
    assert_eq!(out["unsure_proposed"], serde_json::json!([]));
    // Nothing kept yet: no judge call.
    assert_eq!(out["first"]["kept"], true);
    assert_eq!(out["first_asked"], serde_json::json!([]));
    assert_eq!(out["unmarked"], "a review lesson starts with 'Review lesson:'");
}
