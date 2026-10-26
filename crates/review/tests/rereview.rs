//! A re-review settles what the review before it raised instead of starting over (boost-capital/aqua-parser#90 and
//! #91: 22 change requests over two pull requests, most from whole new reviews of commits already reviewed, each
//! finding new medium findings in code no commit had touched). Against the fake hub and a scripted engine, plus the
//! engine's re-check through Python with a scripted frame.

mod github_common;

use github_common::*;
use review::promotion::changed_new_lines;
use review::reviewer::{Outcome, ReviewOptions, Reviewed, review_pull};
use serde_json::{Value, json};

fn posted(outcome: Outcome) -> Reviewed {
    match outcome {
        Outcome::Posted(reviewed) => *reviewed,
        other => panic!("not posted: {other:?}"),
    }
}

fn run(context: &Context, reasons: &[&str]) -> Outcome {
    review_pull(&context.deps, &context.candidate(reasons), ReviewOptions::default()).expect("review")
}

fn engine(results: Vec<Value>) -> std::sync::Arc<FakeEngine> {
    FakeEngine::new(results.into_iter().map(Ok).collect())
}

fn earlier(id: &str, status: &str) -> Value {
    json!({ "id": id, "file": "calc.py", "line": 4, "claim": "total() skips the last item.", "severity": "major",
        "status": status, "evidence": "" })
}

/// A pull request at `sha` reviewed once (requested), with `results` scripted for this and later reviews.
fn reviewed_at(sha: String, results: Vec<Value>) -> Context {
    let context = setup(json!({}), Some(engine(results)));
    let mut pull = FakePull::new("o", "r", 1).requested(&[BOT]);
    pull.head_sha = sha;
    context.hub.add_pull(pull);
    run(&context, &["review_requested"]);
    context
}

fn mention(context: &Context, at: &str) {
    context.hub.with_pull(|pull| {
        pull.issue_comments.push(FakeComment::new(
            900,
            "alice",
            &format!("@{BOT} answered, please look again"),
            at,
        ))
    });
}

/// The head moves from `old()` to `new()`, the increment changing line 4 of calc.py.
fn push(context: &Context) {
    context.hub.with_pull(|pull| {
        pull.head_sha = new();
        pull.ancestors = vec![old()];
        pull.diffs
            .insert(format!("{}..{}", old(), new()), DIFF.replace("- 1)", "- 2)"));
    });
    context.hub.set_now("2026-10-04T11:00:00Z");
}

#[test]
fn review_rereview_a_mention_on_a_commit_reviewed_in_full_rechecks_only_its_findings_with_their_replies() {
    let context = reviewed_at(
        head(),
        vec![
            engine_result(json!({ "findings": [major()] })),
            engine_result(json!({ "earlier": [earlier("aaaaaaa-1", "not_applicable")] })),
        ],
    );
    assert_eq!(context.pull_state().last_review_complete, Some(true));
    let own = context
        .hub
        .pull()
        .review_comments
        .into_iter()
        .find(|comment| comment.user == BOT)
        .expect("inline comment");
    context.hub.with_pull(|pull| {
        let mut reply = FakeComment::new(
            950,
            "alice",
            "The last row is the total line, so stopping one short is right.",
            "2026-10-04T10:20:00Z",
        );
        reply.path = Some("calc.py".into());
        reply.line = Some(4);
        reply.in_reply_to = Some(own.id);
        pull.review_comments.push(reply);
    });
    context.hub.set_now("2026-10-04T11:00:00Z");
    mention(&context, "2026-10-04T10:30:00Z");

    let outcome = posted(run(&context, &["mention"]));
    let spec = &context.engine.specs()[1];
    assert_eq!(spec["recheckOnly"], json!(true));
    assert_eq!(spec["diff"], json!(""));
    assert_eq!(spec["runTests"], json!(false));
    assert_eq!(
        spec["earlier"][0]["replies"],
        json!([{ "author": "alice", "body": "The last row is the total line, so stopping one short is right." }])
    );
    // The disputed finding is settled, nothing new was looked for: the block is replaced by an approval.
    assert_eq!(outcome.plan.event, "APPROVE");
    assert_eq!(context.pull_state().findings[0].status, "not_applicable");
    assert!(
        context
            .logs()
            .contains("aaaaaaa was reviewed in full: re-checking the earlier findings only")
    );
}

#[test]
fn review_rereview_a_commit_reviewed_incompletely_is_read_again_and_what_is_new_there_does_not_block() {
    let context = reviewed_at(
        head(),
        vec![
            engine_result(json!({ "complete": false, "incomplete": ["2 reviewer passes failed"] })),
            engine_result(json!({ "findings": [major()] })),
        ],
    );
    context.hub.set_now("2026-10-04T11:00:00Z");
    mention(&context, "2026-10-04T10:30:00Z");

    let outcome = posted(run(&context, &["mention"]));
    let spec = &context.engine.specs()[1];
    assert!(spec.get("recheckOnly").is_none());
    assert_eq!(spec["diff"], json!(DIFF));
    assert_eq!(outcome.plan.event, "COMMENT");
    let body = &outcome.plan.body;
    assert!(
        body.contains("**Comment.** Nothing that changed since the last review blocks the merge.\n"),
        "{body}"
    );
    assert!(
        body.contains(
            "1 finding at medium or above is on lines that did not change since the last review, so it does not block. \
             1 non-blocking note inline."
        ),
        "{body}"
    );
}

#[test]
fn review_rereview_a_new_finding_outside_what_changed_blocks_only_when_critical() {
    let late = |level: &str| {
        major_with(
            json!({ "file": "util.py", "line": 30, "level": level, "claim": "ratio() divides by zero.",
            "replacement": null }),
        )
    };
    for (level, event) in [("high", "COMMENT"), ("critical", "REQUEST_CHANGES")] {
        let context = reviewed_at(
            old(),
            vec![
                engine_result(json!({ "findings": [major()] })),
                engine_result(json!({ "earlier": [earlier("ccccccc-1", "fixed")], "findings": [late(level)] })),
            ],
        );
        push(&context);
        let outcome = posted(run(&context, &["requested_changes"]));
        assert_eq!(outcome.plan.event, event, "{level}: {}", outcome.plan.body);
    }
    // A finding on the lines the push changed still blocks.
    let context = reviewed_at(
        old(),
        vec![
            engine_result(json!({ "findings": [major()] })),
            engine_result(json!({ "earlier": [earlier("ccccccc-1", "fixed")],
                "findings": [major_with(json!({ "claim": "total() now skips two items." }))] })),
        ],
    );
    push(&context);
    assert_eq!(
        posted(run(&context, &["requested_changes"])).plan.event,
        "REQUEST_CHANGES"
    );
}

#[test]
fn review_rereview_a_comment_that_blocks_nothing_dismisses_the_accounts_request_for_changes() {
    let context = reviewed_at(
        old(),
        vec![
            engine_result(json!({ "findings": [major()] })),
            engine_result(json!({ "complete": false, "incomplete": ["2 reviewer passes failed"],
                "earlier": [earlier("ccccccc-1", "fixed")] })),
        ],
    );
    assert_eq!(context.hub.pull().reviews[0].state, "CHANGES_REQUESTED");
    push(&context);
    let outcome = posted(run(&context, &["requested_changes"]));
    assert_eq!(outcome.plan.event, "COMMENT");
    let reviews = context.hub.pull().reviews;
    assert_eq!(
        reviews.iter().map(|r| r.state.as_str()).collect::<Vec<_>>(),
        ["DISMISSED", "COMMENTED"]
    );
    let dismissals = context.hub.api(&format!(
        r"^PUT repos/o/r/pulls/1/reviews/{}/dismissals$",
        reviews[0].id
    ));
    assert_eq!(
        dismissals[0].body.as_ref().expect("body"),
        &json!({ "message": "Lifted by the review of `ddddddd`: none of its findings blocks the merge.",
            "event": "DISMISS" })
    );
    assert!(context.logs().contains("dismissed its request for changes at ccccccc"));

    // A COMMENT that still names a blocking finding is a request for changes, and nothing is dismissed: only a
    // review whose verdict is COMMENT lifts a block.
    let blocked = reviewed_at(
        old(),
        vec![
            engine_result(json!({ "findings": [major()] })),
            engine_result(json!({ "earlier": [earlier("ccccccc-1", "still_present")] })),
        ],
    );
    push(&blocked);
    assert_eq!(
        posted(run(&blocked, &["requested_changes"])).plan.event,
        "REQUEST_CHANGES"
    );
    assert!(blocked.hub.api("dismissals").is_empty());
}

#[test]
fn review_rereview_a_review_github_created_before_answering_422_is_not_posted_again() {
    let context = setup(
        json!({}),
        Some(engine(vec![engine_result(json!({ "findings": [major()] }))])),
    );
    context.hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    context
        .hub
        .fail_after("POST repos/o/r/pulls/1/reviews", 422, "Unprocessable Entity");
    let outcome = posted(run(&context, &["review_requested"]));
    assert_eq!(context.hub.api(r"^POST repos/o/r/pulls/1/reviews$").len(), 1);
    assert_eq!(context.hub.pull().reviews.len(), 1);
    assert_eq!(outcome.review_id, Some(context.hub.pull().reviews[0].id));
    assert!(context.logs().contains("but created the review; not posting it again"));
}

#[test]
fn review_rereview_changed_new_lines_are_the_added_lines_and_where_lines_were_removed() {
    let diff = "diff --git a/calc.py b/calc.py\n--- a/calc.py\n+++ b/calc.py\n@@ -1,6 +1,7 @@\n def total(items):\n\
                -    x = 1\n+    x = 2\n+    y = 3\n     return x\n@@ -20,3 +21,2 @@\n a\n-b\n c\n\
                diff --git a/new.py b/new.py\nnew file mode 100644\n--- /dev/null\n+++ b/new.py\n@@ -0,0 +1,2 @@\n+one\n+two\n";
    let lines = changed_new_lines(diff);
    assert_eq!(
        lines.into_iter().collect::<Vec<_>>(),
        [
            ("calc.py".to_string(), vec![(2, 3), (22, 22)]),
            ("new.py".to_string(), vec![(1, 2)])
        ]
    );
}

/// The engine's re-check of a finding the author answered in its thread: the reply reaches the frame even though no
/// code changed, and a "not applicable" stands only when it quotes the source.
#[test]
fn review_rereview_the_engine_weighs_a_reply_against_the_source() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join("calc.py"),
        "def total(items):\n    result = 0\n    for i in range(len(items) - 1):\n        result += items[i]\n    return result\n",
    )
    .expect("source");
    let empty = dir.path().join("empty.diff");
    std::fs::write(&empty, "").expect("diff");
    let rlm = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm");
    let script = format!(
        "import sys, json, asyncio\nsys.path.insert(0, {rlm:?})\nimport autoreview_api as a\n\
         class Rlm:\n    def __init__(self, evidence):\n        self.evidence, self.seen = evidence, []\n\
         \x20   async def map(self, tasks, items, **options):\n        self.seen.append(items[0])\n\
         \x20       return [{{'status': 'not_applicable', 'evidence': self.evidence, 'line': 3}}]\n\
         spec = {{'workDir': {work:?}, 'diffPath': {diff:?}, 'earlierDiffPath': {diff:?}, 'recheckOnly': True,\n\
         \x20       'earlier': [{{'id': 'aaaaaaa-1', 'file': 'calc.py', 'line': 3, 'severity': 'major',\n\
         \x20                     'claim': 'total() skips the last item.',\n\
         \x20                     'replies': [{{'author': 'alice', 'body': 'The last row is the total line.'}}]}}]}}\n\
         out = []\n\
         for evidence in ('`for i in range(len(items) - 1):` stops before the total row', 'the author says so'):\n\
         \x20   rlm = Rlm(evidence)\n\
         \x20   result = asyncio.run(a.run(rlm, spec))\n\
         \x20   out.append([result['complete'], result['earlier'][0]['status'],\n\
         \x20               any('The last row is the total line.' in view for view in rlm.seen[0])])\n\
         print(json.dumps(out))\n",
        rlm = rlm.display().to_string(),
        work = dir.path().display().to_string(),
        diff = empty.display().to_string(),
    );
    let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    let output = std::process::Command::new(python)
        .arg("-I")
        .arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("python runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let got: Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(
        got,
        json!([[true, "not_applicable", true], [true, "still_present", true]])
    );
}
