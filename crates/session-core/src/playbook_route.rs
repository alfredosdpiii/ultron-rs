//! Playbook routing (Rust-only, engineering mode): a user message that asks for engineering work opens its run with
//! the rigor playbook that fits it, whole, before the first edit.
//!
//! Problem: the prompt and the situational skills name rigor and its playbooks, and a model did not read them: not
//! from the prompt, not from a pointer, not from the playbook list given after its edits. What arrived before the
//! action it governs (the refused commit) changed what the model did; what arrived after it did not.
//!
//! Fix: the request is matched against keyword routes ([`ROUTES`], first match wins) and the run opens with an
//! `ultron.playbook` message holding that playbook's steps. A playbook already shown since the last compaction is not
//! shown again.
//!
//! Two rules keep the keywords to the request itself. A question is not a request to change code ("whats your plan
//! to fix?"), so it opens no playbook that changes code and falls to Investigation. And a long message is mostly
//! pasted material (a web page, a traceback, a report) whose words are not the user's, so only its first and last
//! [`EDGE_WORDS`] words are read: the request stands before or after the paste.

use regex::Regex;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::LazyLock;

pub const PLAYBOOK_CUSTOM_TYPE: &str = "ultron.playbook";
/// A playbook longer than this is named, not added whole.
pub const MAX_PLAYBOOK_CHARS: usize = 5000;

/// (playbook file stem, title, request pattern), in priority order.
// ponytail: keyword routes, not a model call; a wrong pick costs one read of the right playbook. Ask a small
// model when these still misroute: on 217 real messages they route 92, and the rest are mostly short follow-ups.
pub const ROUTES: &[(&str, &str, &str)] = &[
    (
        "babysit",
        "Babysit",
        r"(?i)\b(babysit|get (this|the) pr green|fix ci|address review comments?|resolve review threads?)\b",
    ),
    ("shipping", "Shipping", r"(?i)\b(land|merge|ship) (the|this) stack\b"),
    (
        "opening-a-pr",
        "Opening a PR",
        r"(?i)\b((open|create|submit|file) (a |the )?(pr|pull request)|gh pr create)\b",
    ),
    (
        "worktree-cleanup",
        "Worktree cleanup",
        r"(?i)\b((clean ?up|prune|remove) (old |stale |merged )?worktrees?)\b",
    ),
    (
        "authoring-a-skill",
        "Authoring a skill",
        r"(?i)\b((write|create|edit|author|update) (a |the )?(new )?(skill|SKILL\.md))\b",
    ),
    (
        "eval",
        "Eval",
        r"(?i)\b(run an? eval|eval(uate)? (this|the) (skill|prompt|change))\b",
    ),
    (
        "runtime-forensics",
        "Runtime forensics",
        r"(?i)\b(memory leak|idle spin|cpu spike|deadlock|hang(s|ing)? indefinitely)\b",
    ),
    (
        "trace-forensics",
        "Trace forensics",
        r"(?i)\b(flamegraph|heap snapshot|cpu profile|perf trace)\b",
    ),
    (
        "prototype",
        "Prototype",
        r"(?i)\b(prototype|quick spike|throwaway sketch)\b",
    ),
    (
        "multi-phase-plan",
        "Multi-phase plan",
        r"(?i)\b(multi-?phase plan|stacked prs?|migration plan)\b",
    ),
    (
        "bug-fix",
        "Bug fix",
        r"(?i)\b(fix(es)?|bug|broken|crash(es|ing)?|regression|fails?|failing|doesn'?t work|not working|wrong)\b",
    ),
    (
        "perf-issue",
        "Perf issue",
        r"(?i)\b(slow(er|ness)?|perf(ormance)?|latency|speed (it )?up|faster)\b",
    ),
    (
        "refactoring",
        "Refactoring",
        r"(?i)\b(refactor\w*|rename\w*|extract|inline|restructure|reorgani[sz]e|clean ?up|split (up )?|move \w+ (to|into))\b",
    ),
    (
        "feature",
        "Feature",
        r"(?i)\b(add|implement|build|support|introduce|create|make it|integrate|new (option|flag|command|endpoint|page)|i (want|need) (a|an|it to|ultron to|this to|the \w+ to))\b",
    ),
    (
        "investigation",
        "Investigation",
        r"(?i)\b(how (does|do|is)|why (does|do|is)|investigate|explain|are we sure|check|compare|measure|audit|analy[sz]e|look (at|into)|find out|what (can|else|do|does|is|are)|want to (know|understand))\b",
    ),
];

/// Words read at each end of a message longer than both ends together.
pub const EDGE_WORDS: usize = 40;
/// The playbooks that change code: a question opens none of them.
const CHANGES_CODE: &[&str] = &[
    "babysit",
    "shipping",
    "opening-a-pr",
    "worktree-cleanup",
    "authoring-a-skill",
    "prototype",
    "multi-phase-plan",
    "bug-fix",
    "refactoring",
    "feature",
];
/// Playbooks that do not open a new branch spec before the first edit.
const NO_SPEC: &[&str] = &[
    "investigation",
    "runtime-forensics",
    "trace-forensics",
    "eval",
    "worktree-cleanup",
    "babysit",
    "shipping",
    "opening-a-pr",
];
const INVESTIGATION: (&str, &str) = ("investigation", "Investigation");

static OPENS_AS_QUESTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(so |i see,? |and |but |also |ok(ay)?,? )*(what'?s?|why|how|is|are|was|were|does|did|which|where|who|when|do (we|you|i|they))\b",
    )
    .expect("valid pattern")
});
/// "lets add a flag?" and "fix the parser, and what broke it?" ask for the change.
static OPENS_AS_REQUEST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(ok(ay)?,? |so |and |also |yes,? )*(lets|let's|please|fix|add|implement|build|make|create|rename|refactor|clean ?up|prune|babysit|open|ship|land|prototype|eval(uate)?|investigate|debug|trace|i (want|need) (a|an|it to|ultron to|this to|the \w+ to))\b",
    )
    .expect("valid pattern")
});
/// "did you fix the tests" asks what happened, not for the fix.
static ASKS_FOR_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(so |i see,? |and |but |also |ok(ay)?,? )*(did|is|are|was|were|has|have)\b")
        .expect("valid pattern")
});
/// A document to write is not a feature to build.
static ASKS_FOR_A_DOCUMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(write-?up|slides?|decision matrix|presentation|pdf|diagram)\b").expect("valid pattern")
});
/// "how do I fix the failing test" asks for the fix.
static ASKS_FOR_A_FIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(fix|repair|solve)\b").expect("valid pattern"));

static COMPILED: LazyLock<Vec<(&'static str, &'static str, Regex)>> = LazyLock::new(|| {
    ROUTES
        .iter()
        .map(|(file, title, pattern)| (*file, *title, Regex::new(pattern).expect("valid route")))
        .collect()
});

/// The playbook (file stem, title) for a user message, `None` for a command, a short reply or no match.
pub fn route(text: &str) -> Option<(&'static str, &'static str)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if text.trim_start().starts_with('/') || words.len() < 2 {
        return None;
    }
    let read = if words.len() > 2 * EDGE_WORDS {
        [&words[..EDGE_WORDS], &words[words.len() - EDGE_WORDS..]]
            .concat()
            .join(" ")
    } else {
        words.join(" ")
    };
    if words.len() < 5 && !OPENS_AS_REQUEST.is_match(&read) {
        return None;
    }
    let question = !OPENS_AS_REQUEST.is_match(&read)
        && (read.ends_with('?')
            || ASKS_FOR_STATUS.is_match(&read)
            || (OPENS_AS_QUESTION.is_match(&read) && !ASKS_FOR_A_FIX.is_match(&read)));
    let document = ASKS_FOR_A_DOCUMENT.is_match(&read);
    COMPILED
        .iter()
        .find(|(file, _, pattern)| {
            !(question && CHANGES_CODE.contains(file)) && !(document && *file == "feature") && pattern.is_match(&read)
        })
        .map(|(file, title, _)| (*file, *title))
        .or(question.then_some(INVESTIGATION))
}

/// Playbooks shown on the branch since its last compaction.
pub fn shown(entries: &[Value]) -> Vec<String> {
    session_journal::context::custom_details_since_compaction(entries, PLAYBOOK_CUSTOM_TYPE)
        .into_iter()
        .filter_map(|details| details["playbook"].as_str().map(str::to_string))
        .collect()
}

/// The message that opens a run of `text`: the fitting playbook from `rigor_dir` (the rigor skill's directory),
/// unless `shown` has it or its file is missing.
pub fn playbook_message(text: &str, rigor_dir: &Path, shown: &[String], timestamp: u64) -> Option<Value> {
    let (file, title) = route(text)?;
    if shown.iter().any(|name| name == file) {
        return None;
    }
    let path = rigor_dir.join("playbooks").join(format!("{file}.md"));
    let body = std::fs::read_to_string(&path).ok()?;
    let body = body.trim();
    let dir = rigor_dir.display();
    // Work that changes code starts from what the finished work must do; the pull request's description is built
    // from it (spec_api.py). Reading changes nothing.
    let spec = if NO_SPEC.contains(&file) {
        ""
    } else {
        " Before the first edit, also write what the finished work must do: `import spec_api; \
         print(spec_api.write(title, goal, [one sentence per behaviour a check can show true or false]))`; \
         `help(spec_api)` has the rest."
    };
    let head = format!(
        "[playbook] This request reads as a {title}. Follow rigor's {title} playbook below: put its steps in \
         state[\"plan\"] before your first edit and work through them; a step you skip stays with `skip: <reason>`. \
         Name a better-fitting playbook in one line if this one is wrong. Paths are relative to {dir}.{spec}"
    );
    let content = if body.chars().count() > MAX_PLAYBOOK_CHARS {
        format!("{head}\nRead {} now, before the first edit.", path.display())
    } else {
        format!("{head}\n\n{body}")
    };
    Some(json!({
        "role": "custom",
        "customType": PLAYBOOK_CUSTOM_TYPE,
        "content": content,
        "display": true,
        "details": {"playbook": file},
        "timestamp": timestamp,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playbook_route_picks_the_first_matching_route() {
        assert_eq!(
            route("the login test fails after the upgrade, please fix it").map(|r| r.0),
            Some("bug-fix")
        );
        assert_eq!(
            route("Rename formatMessageTime to formatTimestamp and update every caller").map(|r| r.0),
            Some("refactoring")
        );
        assert_eq!(
            route("add a --json flag to the usage command").map(|r| r.0),
            Some("feature")
        );
        assert_eq!(
            route("how does the session worker start up").map(|r| r.0),
            Some("investigation")
        );
        assert_eq!(
            route("the dashboard got slow since last week").map(|r| r.0),
            Some("perf-issue")
        );
        assert_eq!(route("ok thanks"), None, "short replies");
        assert_eq!(route("do it all"), None, "casual short replies");
        assert_eq!(route("looks good"), None, "casual acknowledgments");
        assert_eq!(route("fix the build").map(|r| r.0), Some("bug-fix"));
        assert_eq!(route("refactor this module").map(|r| r.0), Some("refactoring"));
        assert_eq!(route("open a pr").map(|r| r.0), Some("opening-a-pr"));
        assert_eq!(route("babysit this pr").map(|r| r.0), Some("babysit"));
        assert_eq!(route("ship the stack").map(|r| r.0), Some("shipping"));
        assert_eq!(route("prune stale worktrees").map(|r| r.0), Some("worktree-cleanup"));
        assert_eq!(route("run an eval on this skill").map(|r| r.0), Some("eval"));
        assert_eq!(
            route("debug the memory leak in worker").map(|r| r.0),
            Some("runtime-forensics")
        );
        assert_eq!(route("/model claude opus five now please"), None, "commands");
        assert_eq!(route("summarize what we did in this session today"), None, "no route");
    }

    #[test]
    fn playbook_route_reads_a_question_as_an_investigation() {
        let name = |text: &str| route(text).map(|r| r.0);
        for text in [
            "check why the nightly job stopped running",
            "compare our reviews with the other reviewers",
            "measure how often memory recalls something useful",
            "what can we take from this repository",
            "whats your plan to fix?",
            "so the import will fail if no session is open?",
            "what is wrong with the parser output",
            "look at the recent commits to see what is already fixed",
        ] {
            assert_eq!(name(text), Some("investigation"), "{text}");
        }
        assert_eq!(
            name("why are we slower than the other reviewer?"),
            Some("perf-issue"),
            "a question still opens a playbook that fits it better"
        );
        for text in [
            "check why the build fails and fix it",
            "how do I fix the failing login test",
            "lets fix the flaky upload test?",
        ] {
            assert_eq!(name(text), Some("bug-fix"), "{text}");
        }
        assert_eq!(name("lets add a --json flag to usage?"), Some("feature"));
    }

    #[test]
    fn playbook_route_tells_a_request_from_a_status_question_or_a_document() {
        let name = |text: &str| route(text).map(|r| r.0);
        assert_eq!(
            name("did you fix the tests and the other finding"),
            Some("investigation")
        );
        assert_eq!(
            name("fix the contradictions but leave it to the model, would this work?"),
            Some("bug-fix")
        );
        assert_eq!(
            name("I want the observer to tell the main agent its findings"),
            Some("feature")
        );
        assert_eq!(name("lets integrate memory into self review?"), Some("feature"));
        assert_eq!(name("create a decision matrix for the three options"), None);
        assert_eq!(name("do what you think is best here"), None);
    }

    #[test]
    fn playbook_route_reads_only_the_ends_of_a_long_message() {
        let name = |text: &str| route(text).map(|r| r.0);
        let menu = "Product Pricing Docs Blog Careers Contact ".repeat(8);
        let pasted = "the build fails with a crash in the parser ".repeat(20);
        let footer = "Terms Privacy Status Security All rights reserved ".repeat(8);
        assert_eq!(name(&format!("{menu}{pasted}{footer}")), None, "a pasted page");
        assert_eq!(
            name(&format!("{menu}{pasted}{footer}are we doing something similar here?")),
            Some("investigation"),
            "the question after the paste"
        );
        assert_eq!(
            name(&format!("{menu}{pasted}{footer}fix this when I switch models")),
            Some("bug-fix"),
            "the request after the paste"
        );
        assert_eq!(
            name(&format!("the upload test fails, the log is below {footer}{footer}")),
            Some("bug-fix"),
            "the request before the paste"
        );
    }

    #[test]
    fn playbook_route_opens_once_with_the_playbook_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("playbooks")).unwrap();
        std::fs::write(
            dir.path().join("playbooks/refactoring.md"),
            "### Refactoring\n\n1. Pin the contract.\n",
        )
        .unwrap();
        let text = "Rename formatMessageTime to formatTimestamp in lib/date-utils.ts";
        let message = playbook_message(text, dir.path(), &[], 7).unwrap();
        let content = message["content"].as_str().unwrap();
        assert!(
            content.starts_with("[playbook] This request reads as a Refactoring."),
            "{content}"
        );
        assert!(
            content.ends_with("### Refactoring\n\n1. Pin the contract."),
            "{content}"
        );
        assert!(
            content.contains("spec_api.write("),
            "work that changes code starts from a spec: {content}"
        );
        std::fs::write(dir.path().join("playbooks/investigation.md"), "### Investigation\n").unwrap();
        let reading = playbook_message("how does the session worker start up", dir.path(), &[], 7).unwrap();
        assert!(
            !reading["content"].as_str().unwrap().contains("spec_api"),
            "reading changes nothing: {reading}"
        );
        let entries = [json!({"type": "message", "message": message})];
        assert_eq!(shown(&entries), ["refactoring"]);
        assert_eq!(playbook_message(text, dir.path(), &shown(&entries), 8), None, "once");
        let compacted = [entries[0].clone(), json!({"type": "compaction"})];
        assert!(
            playbook_message(text, dir.path(), &shown(&compacted), 9).is_some(),
            "again after a compaction"
        );
        assert_eq!(
            playbook_message("fix the crash in the parser module now", dir.path(), &[], 7),
            None,
            "missing file"
        );
    }
}
