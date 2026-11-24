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
        "def read_receipt():",
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
        "out['clean'] = read_receipt()",
        "out['empty_reason'] = error(lambda: r.waive('  ', cwd=d))",
        "out['waived'] = r.waive('a follow-up', cwd=d)",
        "out['after_waive'] = read_receipt()",
        "os.remove(receipt)",
        "partial = await r.run(rlm, '--model fake/model --only bugs', cwd=d)",
        "out['only_bugs'] = read_receipt()",
        "open(os.path.join(d, 'a.py'), 'w').write('two\\n')",
        "scope = r.resolve_scope(r.parse_args('', {}), d)",
        "out['dirty'] = r._write_receipt(r.Git(d), scope, r.parse_args('', {}), 0, False)",
        "out['dirty_receipt'] = read_receipt()",
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
fn review_rounds_are_counted_per_branch() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py");
    let handler: Arc<dyn HostHandler> = Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null));
    let kernel = Kernel::new(KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime), handler).unwrap();
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.py"), "one\n").unwrap();
    git(dir, &["add", "a.py"]);
    git(dir, &["commit", "-qm", "one"]);
    git(dir, &["checkout", "-qb", "feature"]);
    let code = [
        "import json, os, subprocess, review_api as r",
        &format!("d = {:?}", dir.display().to_string()),
        "receipt = os.path.join(d, '.git', 'ultron-review', 'receipt.json')",
        "def commit(text, branch=None):",
        "    if branch:",
        "        subprocess.run(['git', 'checkout', '-qb', branch, 'main'], cwd=d, check=True)",
        "    open(os.path.join(d, 'a.py'), 'w').write(text)",
        "    subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@x', '-c', 'commit.gpgsign=false',",
        "                    'commit', '-qam', text], cwd=d, check=True)",
        "def review(confirmed=1):",
        "    options = r.parse_args('', {})",
        "    r._write_receipt(r.Git(d), r.resolve_scope(options, d), options, confirmed, False)",
        "    return json.load(open(receipt))['round']",
        "out = []",
        "commit('two\\n')",
        "out.append(review())",
        "out.append(review())",
        "commit('three\\n')",
        "out.append(review())",
        "commit('four\\n')",
        "out.append(review())",
        "commit('five\\n', branch='other')",
        "out.append(review())",
        "data = json.load(open(receipt))",
        "del data['round']",
        "json.dump(data, open(receipt, 'w'))",
        "commit('six\\n')",
        "out.append(review())",
        "print(json.dumps(out))",
    ]
    .join("\n");
    let result = kernel.execute(&code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let rounds: Value = serde_json::from_str(result.stdout.trim()).unwrap();
    // First review, the same tree again, a fix, another fix, another branch, a receipt from before rounds.
    assert_eq!(rounds, serde_json::json!([1, 1, 2, 3, 1, 2]));
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
        "mem, jdg = Memory(kept), Judge('guard removed')",
        "out['duplicate'] = await r.keep_lesson(mem, jdg, new, ref, key='review-lesson-tests')",
        "out['duplicate_proposed'], out['asked'] = mem.proposed, jdg.asked",
        "mem = Memory(kept)",
        "out['new'] = await r.keep_lesson(mem, Judge('nothing alike'), new, ref, key='review-lesson-tests')",
        "out['new_proposed'] = mem.proposed",
        "mem = Memory(kept)",
        "out['unsure'] = await r.keep_lesson(mem, Judge('guard removed', settled=False), new, ref)",
        "out['unsure_proposed'] = mem.proposed",
        "mem, jdg = Memory([]), Judge('x')",
        "out['first'] = await r.keep_lesson(mem, jdg, new, ref)",
        "out['first_asked'] = jdg.asked",
        "try:",
        "    await r.keep_lesson(mem, jdg, 'Tests are good.', ref)",
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

#[test]
fn review_holds_new_definitions_against_what_the_repository_already_has() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py");
    let handler: Arc<dyn HostHandler> = Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null));
    let kernel = Kernel::new(KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime), handler).unwrap();
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q"]);
    let old = "def parse_money(text):\n    cents = text.replace('$', '')\n    return int(float(cents) * 100)\n";
    std::fs::write(dir.join("money.py"), old).unwrap();
    std::fs::write(dir.join("notes.py"), "limit = 1\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "one"]);
    // The change: a second money parser under another name, a fallback that names nothing, and an edit with neither.
    let new = "def parse_money_text(value):\n    cleaned = value.replace('$', '')\n    return int(float(cleaned) * 100)\n\n\n\
               def total(rows):\n    # legacy rows carry strings: fall back to parsing them\n    \
               return sum(parse_money_text(row) for row in rows)\n";
    std::fs::write(dir.join("totals.py"), new).unwrap();
    std::fs::write(dir.join("notes.py"), "limit = 2\n").unwrap();
    let code = [
        "import json, review_api as r",
        &format!("d = {:?}", dir.display().to_string()),
        "scope = r.resolve_scope(r.parse_args('', {}), d)",
        "inventory, inv_notes = r.reuse_inventory(r.Git(d), scope)",
        "out = {'files': sorted(inventory), 'entries': '\\n'.join(inventory.get('totals.py', [])), 'notes': inv_notes}",
        "reviewer = r.reuse_reviewer(inventory)",
        "chunks, _ = r.build_chunks(scope.files, scope.read_file)",
        "plan = r.plan_find(chunks, [*r.parse_args('', {}).reviewers, reviewer], 100000)",
        "out['reuse_frames'] = [chunk.path for item, chunk in plan['frames'] if item.key == 'reuse']",
        "out['task'] = r.finder_task(reviewer)",
        "finding = {'line': 1, 'severity': 'minor', 'category': 'duplication', 'confidence': 0.9,",
        "           'claim': 'parse_money_text repeats parse_money', 'why': 'a fix to one misses the other',",
        "           'suggested_fix': 'call parse_money'}",
        "class Fake:",
        "    def __init__(self):",
        "        self.tasks, self.views = [], []",
        "    async def map(self, tasks, items, **kwargs):",
        "        if isinstance(tasks, str):",
        "            self.views += items",
        "            return [{'verdict': 'confirmed', 'evidence': '`def parse_money_text(value):` repeats money.py'}",
        "                    for _ in items]",
        "        self.tasks += tasks",
        "        return [[finding] if 'Reuse and compatibility' in task and 'File: totals.py' in item else []",
        "                for task, item in zip(tasks, items)]",
        "    async def infer(self, task, context, **kwargs):",
        "        return []",
        "fake = Fake()",
        "out['report'] = (await r.run(fake, '--model fake/model', cwd=d)).report",
        "out['reuse_tasks'] = sum('Reuse and compatibility' in task for task in fake.tasks)",
        "out['verifier_views'] = fake.views",
        "fake = Fake()",
        "await r.run(fake, '--model fake/model --only bugs', cwd=d)",
        "out['only_bugs'] = sum('Reuse and compatibility' in task for task in fake.tasks)",
        "print(json.dumps(out))",
    ]
    .join("\n");
    let result = kernel.execute(&code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let out: Value = serde_json::from_str(result.stdout.trim()).unwrap();
    let text = |key: &str| out[key].as_str().unwrap_or_else(|| panic!("{key}: {out}")).to_string();

    // The inventory is made by searching, before any model is asked: the new definition with the one it resembles,
    // and the added line that speaks of a fallback. A file with neither gets no frame.
    assert_eq!(out["files"], serde_json::json!(["totals.py"]), "{out}");
    let entries = text("entries");
    assert!(entries.contains("totals.py:1 new `parse_money_text`"), "{entries}");
    assert!(entries.contains("money.py:1: def parse_money(text):"), "{entries}");
    assert!(
        entries.contains("return int(float(cents) * 100)"),
        "the existing body is shown: {entries}"
    );
    assert!(entries.contains("totals.py:7 "), "the fallback line: {entries}");
    assert!(
        !entries.contains("`total`"),
        "a one-word name is not searched: {entries}"
    );
    assert_eq!(out["notes"], serde_json::json!([]), "{out}");
    assert_eq!(out["reuse_frames"], serde_json::json!(["totals.py"]), "{out}");
    assert!(text("task").contains("money.py:1: def parse_money(text):"), "{out}");

    // A whole review asks the reuse finder once, and the verifier of its finding sees the existing definition.
    assert_eq!(out["reuse_tasks"], 1, "{out}");
    assert_eq!(out["only_bugs"], 0, "chosen reviewers only: {out}");
    let views = out["verifier_views"][0].as_array().unwrap_or_else(|| panic!("{out}"));
    assert!(
        views
            .iter()
            .any(|view| view.as_str().unwrap().contains("money.py:1: def parse_money(text):")),
        "{views:?}"
    );
    assert!(text("report").contains("parse_money_text repeats parse_money"), "{out}");
}

/// A finder sees one chunk and the verifier a window, so neither knows what the endpoint a change calls answers,
/// nor the code a verifier says it lacks: a search and one more ask supply both. Small findings do not hold a ship.
#[test]
fn review_reads_the_code_a_change_talks_to() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py");
    let handler: Arc<dyn HostHandler> = Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null));
    let kernel = Kernel::new(KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime), handler).unwrap();
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q"]);
    let server = "@app.post(\"/hitl/approve\")\nasync def approve(request):\n    record(request)\n    if overruled:\n        \
                  return {\"success\": True, \"status\": \"DENIED\"}\n    return {\"success\": True, \"status\": \"APPROVED\"}\n";
    std::fs::write(dir.join("server.py"), server).unwrap();
    let old = "const reply = await post(\"/hitl/approve\", body);\nif (reply.status === \"DENIED\") showOverruled();\n";
    std::fs::write(dir.join("old_page.js"), old).unwrap();
    let hooks = "def use_console_data():\n    data = None  # cleared when a load fails\n    return data\n";
    std::fs::write(dir.join("hooks.py"), hooks).unwrap();
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    let ci = "jobs:\n  lint:\n    steps:\n      - run: pip install ruff\n      - run: ruff check backend/\n      \
              - run: ruff format --check backend/\n";
    std::fs::write(dir.join(".github/workflows/ci.yml"), ci).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "one"]);
    let page = "export function approve(body) {\n  return post(\"/hitl/approve\", body).then(() => \"approved\");\n}\n";
    std::fs::write(dir.join("page.js"), page).unwrap();
    let code = [
        "import json, review_api as r",
        &format!("d = {:?}", dir.display().to_string()),
        "scope = r.resolve_scope(r.parse_args('', {}), d)",
        "inventory, inv_notes = r.contract_inventory(r.Git(d), scope)",
        "out = {'files': sorted(inventory), 'entries': '\\n'.join(inventory.get('page.js', [])), 'notes': inv_notes}",
        "reviewer = r.contract_reviewer(inventory)",
        "chunks, _ = r.build_chunks(scope.files, scope.read_file)",
        "plan = r.plan_find(chunks, [*r.parse_args('', {}).reviewers, reviewer], 100000)",
        "out['frames'] = [chunk.path for item, chunk in plan['frames'] if item.key == 'contract']",
        "finding = {'line': 2, 'severity': 'minor', 'category': 'correctness', 'confidence': 0.9,",
        "           'claim': 'the pane goes blank after a failed reload', 'why': 'the hook clears its data',",
        "           'suggested_fix': 'keep the last data'}",
        "class Fake:",
        "    def __init__(self, sure):",
        "        self.sure, self.asks = sure, []",
        "    async def map(self, tasks, items, **kwargs):",
        "        if not isinstance(tasks, str):",
        "            return [[finding] if 'Correctness' in task else [] for task in tasks]",
        "        self.asks.append(items)",
        "        if self.sure and any('cleared when a load fails' in view for view in items[0]):",
        "            return [{'verdict': 'confirmed', 'evidence': '`data = None  # cleared when a load fails`'}]",
        "        return [{'verdict': 'uncertain', 'evidence': 'use_console_data is not in the views',",
        "                 'needs': ['use_console_data']} for _ in items]",
        "    async def infer(self, task, context, **kwargs):",
        "        return []",
        "fake = Fake(True)",
        "out['report'] = (await r.run(fake, '--model fake/model', cwd=d)).report",
        "out['asks'] = len(fake.asks)",
        "out['second'] = fake.asks[1][0]",
        "out['receipt'] = json.loads(open(r._receipt_path(r.Git(d))).read())",
        "fake = Fake(False)",
        "out['unsure'] = (await r.run(fake, '--model fake/model', cwd=d)).report",
        "out['unsure_asks'] = len(fake.asks)",
        "print(json.dumps(out))",
    ]
    .join("\n");
    let result = kernel.execute(&code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let out: Value = serde_json::from_str(result.stdout.trim()).unwrap();
    let text = |key: &str| out[key].as_str().unwrap_or_else(|| panic!("{key}: {out}")).to_string();

    // The endpoint the new page calls, with what its handler answers and how the page it replaces reads the answer.
    assert_eq!(out["files"], serde_json::json!(["page.js"]), "{out}");
    let entries = text("entries");
    assert!(entries.contains("server.py:1"), "{entries}");
    assert!(
        entries.contains("\"status\": \"DENIED\""),
        "what the handler returns: {entries}"
    );
    assert!(entries.contains("old_page.js:1"), "the other caller: {entries}");
    assert!(entries.contains("showOverruled"), "{entries}");
    assert_eq!(out["frames"], serde_json::json!(["page.js"]), "{out}");

    // A verifier that names the code it lacks is asked once more, with that code.
    assert_eq!(out["asks"], 2, "{out}");
    let second = out["second"].as_array().unwrap_or_else(|| panic!("{out}"));
    assert!(
        second.iter().any(
            |view| view.as_str().unwrap().contains("hooks.py:1") && view.as_str().unwrap().contains("cleared when")
        ),
        "{second:?}"
    );
    let report = text("report");
    assert!(report.contains("## Minor (1)"), "{report}");
    assert!(!report.contains("## Uncertain"), "{report}");
    // A minor finding is reported and does not hold the pull request.
    assert_eq!(out["receipt"]["confirmed"], 0, "{out}");
    assert_eq!(out["receipt"]["minor"], 1, "{out}");
    // What CI will run is named where the report says nothing was run.
    assert!(report.contains("`ruff format --check backend/`"), "{report}");
    assert!(!report.contains("pip install"), "{report}");

    // Asked once more only: what stays undecided names what to read.
    assert_eq!(out["unsure_asks"], 2, "{out}");
    let unsure = text("unsure");
    assert!(unsure.contains("## Uncertain, not confirmed (1)"), "{unsure}");
    assert!(unsure.contains("read the code it names"), "{unsure}");
}

/// A whole self-review sends cheap sub-agents past the diff: one per changed file to read what the file calls and
/// what it replaces, and one for each serious finding the frames could not decide.
#[test]
fn review_sends_children_past_the_diff() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py");
    let handler: Arc<dyn HostHandler> = Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null));
    let kernel = Kernel::new(KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime), handler).unwrap();
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q"]);
    let server = "@app.post(\"/hitl/approve\")\nasync def approve(request):\n    if overruled:\n        \
                  return {\"success\": True, \"status\": \"DENIED\"}\n    return {\"success\": True, \"status\": \"APPROVED\"}\n";
    std::fs::write(dir.join("server.py"), server).unwrap();
    // The page being replaced calls another route, so no search by route name finds it.
    let old = "const reply = await decide(body);\nif (reply.status === \"DENIED\") showOverruled();\n";
    std::fs::write(dir.join("old_page.js"), old).unwrap();
    let hooks = "def use_console_data():\n    data = None  # cleared when a load fails\n    return data\n";
    std::fs::write(dir.join("hooks.py"), hooks).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "one"]);
    let page = "export function approve(body) {\n  return post(\"/hitl/approve\", body).then(() => \"approved\");\n}\n";
    std::fs::write(dir.join("page.js"), page).unwrap();
    let code = [
        "import json, review_api as r, spec_api",
        &format!("d = {:?}", dir.display().to_string()),
        "spec_api.write('Approvals page', 'Approve from the new page.', ['The page approves a request.'], cwd=d)",
        "found = {'findings': [{'line': 2, 'severity': 'major', 'category': 'correctness', 'confidence': 0.9,",
        "    'claim': 'an overruled approval is announced as approved', 'why': 'old_page.js:2 handles DENIED',",
        "    'suggested_fix': 'read reply.status', 'read': ['old_page.js:2']}]}",
        "class Fake:",
        "    def __init__(self):",
        "        self.spawned, self.asks = [], []",
        "    async def spawn(self, brief, **kwargs):",
        "        self.spawned.append((brief, kwargs))",
        "        return kwargs['name']",
        "    async def results(self, handles):",
        "        verdict = {'verdict': 'confirmed', 'evidence': 'export function approve(body) { drops the status'}",
        "        return {'running': [], 'results': [{'id': h, 'result': {'status': 'succeeded',",
        "                'value': json.dumps(found if h.startswith('review-explore') else verdict)}} for h in handles]}",
        "    async def map(self, tasks, items, **kwargs):",
        "        if not isinstance(tasks, str):",
        "            return [[] for _ in tasks]",
        "        self.asks.append(items)",
        "        asked = '\\n'.join(items[0]).partition('Code you asked for:')[2]",
        "        if 'hooks.py:1' in asked:",
        "            return [{'verdict': 'uncertain', 'evidence': 'cannot tell from these lines'}]",
        "        need = 'hooks.py' if 'server.py:1' in asked else 'server.py'",
        "        return [{'verdict': 'uncertain', 'evidence': 'not in the views', 'needs': [need]}]",
        "    async def infer(self, task, context, **kwargs):",
        "        return []",
        "fake = Fake()",
        "out = {'report': (await r.run(fake, '', cwd=d)).report}",
        "out['names'] = [kwargs['name'] for _, kwargs in fake.spawned]",
        "out['models'] = [kwargs.get('model') for _, kwargs in fake.spawned]",
        "out['brief'] = fake.spawned[0][0]",
        "out['asks'] = len(fake.asks)",
        "out['first'] = fake.asks[0][0]",
        "out['last'] = fake.asks[-1][0]",
        "out['receipt'] = json.loads(open(r._receipt_path(r.Git(d))).read())",
        "out['spec'] = spec_api.read(cwd=d)",
        "await r.run(Fake(), '', cwd=d)",
        "out['spec_again'] = spec_api.read(cwd=d)",
        "out['revised'] = spec_api.write('Approvals page', 'Approve.', ['The page approves.'], cwd=d, revise='wording')",
        "fake = Fake()",
        "await r.run(fake, '--deep=0', cwd=d)",
        "out['off'] = len(fake.spawned)",
        "print(json.dumps(out))",
    ]
    .join("\n");
    let result = kernel.execute(&code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let out: Value = serde_json::from_str(result.stdout.trim()).unwrap();
    let text = |key: &str| out[key].as_str().unwrap_or_else(|| panic!("{key}: {out}")).to_string();

    // One explorer for the changed file, then one check of the major finding the frames left undecided; both on
    // the host's sub-agent model, so none is named.
    assert_eq!(
        out["names"],
        serde_json::json!(["review-explore-1", "review-check-1"]),
        "{out}"
    );
    assert_eq!(out["models"], serde_json::json!([null, null]), "{out}");
    let brief = text("brief");
    assert!(brief.contains("page.js") && brief.contains("/hitl/approve"), "{brief}");
    assert!(brief.contains("replaces"), "{brief}");
    assert!(brief.contains("Do not run tests"), "{brief}");

    // The verifier sees the code the explorer read, found by no search: the page being replaced.
    let first = out["first"].as_array().unwrap_or_else(|| panic!("{out}"));
    assert!(
        first
            .iter()
            .any(|view| view.as_str().unwrap().contains("showOverruled")),
        "{first:?}"
    );
    // It is asked again while it names code it has not seen: the handler, then the hook.
    assert_eq!(out["asks"], 3, "{out}");
    let last = out["last"]
        .as_array()
        .unwrap_or_else(|| panic!("{out}"))
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(last.contains("server.py:1") && last.contains("hooks.py:1"), "{last}");

    let report = text("report");
    assert!(report.contains("## Major (1)"), "{report}");
    assert!(
        report.contains("an overruled approval is announced as approved"),
        "{report}"
    );
    assert!(report.contains("2 sub-agents"), "{report}");
    assert_eq!(out["receipt"]["confirmed"], 1, "{out}");

    // The endpoint the change calls becomes an acceptance item only a run against the real handler passes.
    let spec = text("spec");
    assert!(
        spec.contains("- pending A2 ") && spec.contains("`/hitl/approve`") && spec.contains("server.py:1"),
        "{spec}"
    );
    assert!(report.contains("A2"), "{report}");
    assert_eq!(text("spec_again").matches("/hitl/approve").count(), 1, "{out}");
    assert!(text("revised").contains("`/hitl/approve`"), "{out}");

    assert_eq!(out["off"], 0, "{out}");
}

#[test]
fn review_budget_grows_with_the_change() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py");
    let handler: Arc<dyn HostHandler> = Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null));
    let kernel = Kernel::new(KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime), handler).unwrap();
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q"]);
    for file in 0..30 {
        std::fs::write(dir.join(format!("m{file}.py")), "x = 0\n").unwrap();
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "one"]);
    // A change of 30 files of about 12,000 characters each: four finders on each is far past the 300k default.
    for file in 0..30 {
        let body: String = (0..400)
            .map(|line| format!("value_{file}_{line} = compute({line})\n"))
            .collect();
        std::fs::write(dir.join(format!("m{file}.py")), body).unwrap();
    }
    let code = [
        "import json, review_api as r",
        &format!("d = {:?}", dir.display().to_string()),
        "class Fake:",
        "    def __init__(self):",
        "        self.budgets = []",
        "    async def map(self, tasks, items, **kwargs):",
        "        self.budgets.append(kwargs['budget'].tokens)",
        "        return [[] for _ in items]",
        "    async def infer(self, task, context, **kwargs):",
        "        return []",
        "out = {'plan': (await r.run(Fake(), '--plan', cwd=d)).report}",
        "out['capped'] = (await r.run(Fake(), '--plan --budget 300k', cwd=d)).report",
        "out['paths'] = (await r.run(Fake(), '--plan m0.py m1.py', cwd=d)).report",
        "fake = Fake()",
        "out['report'] = (await r.run(fake, '--model fake/model --deep=0', cwd=d)).report",
        "out['budgets'] = fake.budgets",
        "print(json.dumps(out))",
    ]
    .join("\n");
    let result = kernel.execute(&code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let out: Value = serde_json::from_str(result.stdout.trim()).unwrap();
    let text = |key: &str| out[key].as_str().unwrap_or_else(|| panic!("{key}: {out}")).to_string();

    // With no --budget, a review of the whole local change gets the budget its finder passes need.
    assert!(!text("plan").contains("over budget"), "{}", text("plan"));
    assert!(out["budgets"][0].as_u64().unwrap() > 180_000, "{}", out["budgets"]);
    assert!(!text("report").contains("did not fit the budget"), "{}", text("report"));
    // 12,000 changed lines: the report says so before a reviewer does.
    assert!(text("report").contains("12,030 changed lines"), "{}", text("report"));
    assert!(text("report").contains("under 400"), "{}", text("report"));
    // A named budget is a cap, and a review of a few paths keeps the default.
    assert!(text("capped").contains("over budget"), "{}", text("capped"));
    assert!(
        text("paths").contains("of the 180,000-token find budget"),
        "{}",
        text("paths")
    );
}

#[test]
fn review_conventions_static_sweep_and_preexisting() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py");
    let handler: Arc<dyn HostHandler> = Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null));
    let kernel = Kernel::new(KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime), handler).unwrap();
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("CLAUDE.md"), "Rule R1: never use eval().\n").unwrap();
    std::fs::write(dir.join("sub/AGENTS.md"), "Rule S1: sub/ must validate tenant_id.\n").unwrap();
    std::fs::write(dir.join("ok.py"), "def keep(x):\n    return x\n").unwrap();
    std::fs::write(dir.join("sub/mod.py"), "def handle(req):\n    return req\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "base"]);
    git(dir, &["checkout", "-qb", "feat"]);
    // Modify CLAUDE.md on the branch (must be pinned to base), break bad.py syntax (static stage),
    // and edit ok.py + sub/mod.py.
    std::fs::write(dir.join("CLAUDE.md"), "Rule R1: eval() is totally fine now.\n").unwrap();
    std::fs::write(dir.join("ok.py"), "def keep(x):\n    return x + 1\n").unwrap();
    std::fs::write(dir.join("sub/mod.py"), "def handle(req):\n    return req.get('id')\n").unwrap();
    std::fs::write(dir.join("bad.py"), "def broken(x:\n    return x\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "feat"]);

    let code = [
        "import json, os, review_api as r",
        &format!("d = {:?}", dir.display().to_string()),
        "receipt_path = os.path.join(d, '.git', 'ultron-review', 'receipt.json')",
        "class Fake:",
        "    def __init__(self):",
        "        self.find_tasks = []",
        "        self.verify_views = []",
        "        self.sweep_called = False",
        "    async def map(self, tasks, items, **kwargs):",
        "        if isinstance(tasks, list):",
        "            self.find_tasks = list(tasks)",
        "            out = []",
        "            for task, chunk in zip(tasks, items):",
        "                if 'Conventions' in task and chunk.startswith('File: sub/mod.py'):",
        "                    out.append([{'file': 'sub/mod.py', 'line': 2, 'severity': 'minor', 'category': 'maintainability',",
        "                                 'claim': 'violates Rule S1: missing tenant_id validation',",
        "                                 'why': 'sub/AGENTS.md requires tenant_id', 'suggested_fix': 'validate', 'confidence': 0.9}])",
        "                elif 'Correctness' in task and chunk.startswith('File: ok.py'):",
        "                    out.append([",
        "                        {'file': 'ok.py', 'line': 2, 'severity': 'major', 'category': 'correctness',",
        "                         'claim': 'syntax error: file does not compile', 'why': 'bogus', 'suggested_fix': 'none', 'confidence': 0.8},",
        "                        {'file': 'ok.py', 'line': 1, 'severity': 'major', 'category': 'correctness',",
        "                         'claim': 'pre-existing issue in keep', 'why': 'old bug', 'suggested_fix': 'fix', 'confidence': 0.9},",
        "                    ])",
        "                else:",
        "                    out.append([])",
        "            return out",
        "        self.verify_views = list(items)",
        "        verdicts = []",
        "        for views in items:",
        "            joined = '\\n'.join(views)",
        "            if 'pre-existing issue in keep' in joined:",
        "                verdicts.append({'verdict': 'confirmed', 'evidence': 'def keep(x):', 'corrected_line': None, 'introduced': 'before'})",
        "            elif 'swept bug in sub/mod.py' in joined:",
        "                verdicts.append({'verdict': 'confirmed', 'evidence': \"return req.get('id')\", 'corrected_line': None, 'introduced': 'this_change'})",
        "            else:",
        "                verdicts.append({'verdict': 'confirmed', 'evidence': \"return req.get('id')\", 'corrected_line': None, 'introduced': 'this_change'})",
        "        return verdicts",
        "    async def infer(self, task, views, **kwargs):",
        "        if 'final gap sweep' in task:",
        "            self.sweep_called = True",
        "            return [{'file': 'sub/mod.py', 'line': 2, 'severity': 'minor', 'category': 'correctness',",
        "                     'claim': 'swept bug in sub/mod.py', 'why': 'guard dropped', 'suggested_fix': 'add guard', 'confidence': 0.8}]",
        "        return []",
        "fake = Fake()",
        "rev = await r.run(fake, '--model fake/model --deep=0', cwd=d)",
        "scope = r.resolve_scope(r.parse_args('', {}), d)",
        "conv, conv_notes = r.conventions_inventory(r.Git(d), scope, base=scope.base)",
        "receipt = json.load(open(receipt_path)) if os.path.exists(receipt_path) else None",
        "print(json.dumps({",
        "    'conv_keys': sorted(conv.keys()),",
        "    'ok_conv': '\\n'.join(conv.get('ok.py', [])),",
        "    'sub_conv': '\\n'.join(conv.get('sub/mod.py', [])),",
        "    'conv_notes': conv_notes,",
        "    'sweep_called': fake.sweep_called,",
        "    'verified_claims': [v[0] for v in fake.verify_views],",
        "    'report': rev.report,",
        "    'receipt': receipt,",
        "}))",
    ]
    .join("\n");
    let result = kernel.execute(&code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{result:?}");
    let out: Value = serde_json::from_str(result.stdout.trim()).unwrap();

    // Ancestor scoping + base-commit pinning: ok.py gets root CLAUDE.md (base version!), not sub/AGENTS.md;
    // sub/mod.py gets both sub/AGENTS.md and root CLAUDE.md (base version!).
    let ok_conv = out["ok_conv"].as_str().unwrap();
    let sub_conv = out["sub_conv"].as_str().unwrap();
    assert!(ok_conv.contains("never use eval()"), "{ok_conv}");
    assert!(!ok_conv.contains("totally fine now"), "{ok_conv}");
    assert!(!ok_conv.contains("Rule S1"), "{ok_conv}");
    assert!(sub_conv.contains("Rule S1") && sub_conv.contains("never use eval()"), "{sub_conv}");

    // Gap sweep ran and its candidate went through verification.
    assert_eq!(out["sweep_called"], Value::Bool(true), "{out}");
    let verified_joined = out["verified_claims"].to_string();
    assert!(verified_joined.contains("swept bug in sub/mod.py"), "{verified_joined}");
    // Bogus syntax-error claim on valid ok.py was contradicted by static compile stage before verification.
    assert!(!verified_joined.contains("file does not compile"), "{verified_joined}");

    let report = out["report"].as_str().unwrap();
    assert!(report.contains("bad.py") && report.contains("no longer compiles"), "{report}");
    assert!(report.contains("## Pre-existing, not blocking (1)"), "{report}");
    assert!(report.contains("pre-existing issue in keep"), "{report}");
    assert_eq!(out["receipt"]["confirmed"], 1, "{out}");
}
