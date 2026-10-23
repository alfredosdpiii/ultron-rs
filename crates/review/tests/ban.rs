//! `autoreview.ban` (Rust only): a reviewing account never reviews, comments on or re-reviews a banned author's pull
//! request, and does not act on a banned login's review request or mention. Against the fake hub and a scripted
//! engine; concurrency waits on events (an engine gate, the queue in the state), every deadline is a hang guard.

mod github_common;

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use github_common::*;
use review::accounts::Account;
use review::ban::{ALL, BanList, Bans, add_ban, remove_ban};
use review::daemon::{Daemon, DaemonDeps, Reported};
use review::reviewer::{Outcome, ReviewOptions, review_pull};
use serde_json::{Map, Value, json};

const GUARD: Duration = Duration::from_secs(120);
const BANNED: &str = "lepmoran-boostkh";

type Outcomes = Arc<Mutex<Vec<(String, u64, String)>>>;

fn daemon_with(context: &Context, accounts: Vec<Account>) -> (Daemon, Outcomes) {
    let outcomes: Outcomes = Arc::new(Mutex::new(Vec::new()));
    let sink = outcomes.clone();
    let daemon = Daemon::new(DaemonDeps {
        reviewer: context.deps.clone(),
        accounts,
        sleep: Some(Arc::new(|_| {})),
        on_outcome: Some(Arc::new(move |candidate, outcome: &Reported| {
            sink.lock().expect("outcomes").push((
                candidate.account.login.clone(),
                candidate.pull.number,
                outcome.kind().to_string(),
            ));
        })),
    });
    (daemon, outcomes)
}

fn sorted(outcomes: &Outcomes) -> Vec<(String, u64, String)> {
    let mut all = outcomes.lock().expect("outcomes").clone();
    all.sort();
    all
}

fn outcome(login: &str, number: u64, kind: &str) -> (String, u64, String) {
    (login.into(), number, kind.into())
}

fn by(author: &str, number: u64, requested: &[&str]) -> FakePull {
    let mut pull = FakePull::new("o", "r", number).requested(requested);
    pull.author = author.into();
    pull.head_sha = number.to_string().repeat(40);
    pull
}

fn count(text: &str, line: &str) -> usize {
    text.lines().filter(|logged| *logged == line).count()
}

/// A ban list the test changes while the daemon runs: re-read like the settings file.
fn live_bans(context: &mut Context) -> Arc<Mutex<BanList>> {
    let list = Arc::new(Mutex::new(BanList::default()));
    let read = list.clone();
    context.deps.bans = Arc::new(Bans::reloading(Arc::new(move || {
        Some(read.lock().expect("bans").clone())
    })));
    list
}

fn ban_all(login: &str) -> BanList {
    BanList::from_value(Some(&json!({ ALL: [login] })))
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(start.elapsed() < GUARD, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// An engine hook that says a review started and waits until the test opens the gate.
fn gate(engine: &FakeEngine) -> (Arc<(Mutex<bool>, Condvar)>, mpsc::Receiver<()>) {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let hook_gate = gate.clone();
    let (started, starts) = mpsc::channel::<()>();
    let started = Mutex::new(started);
    *engine.on_review.lock().expect("hook") = Some(Box::new(move || {
        let _ = started.lock().expect("started").send(());
        let (lock, changed) = &*hook_gate;
        let _ = changed
            .wait_timeout_while(lock.lock().expect("gate"), GUARD, |open| !*open)
            .expect("gate");
    }));
    (gate, starts)
}

fn open(gate: &Arc<(Mutex<bool>, Condvar)>) {
    *gate.0.lock().expect("gate") = true;
    gate.1.notify_all();
}

#[test]
fn review_ban_list_matches_accounts_and_logins_case_insensitively() {
    let list = BanList::from_value(Some(&json!({
        "bryandlp-boost": ["Lepmoran-BoostKH", "@other"],
        "GHE.example.com/Work": ["intern"],
        "*": ["spammer"],
        "broken": "not a list",
        "": ["nobody"],
        "empty": [],
    })));
    let boost = Account::new("BryanDLP-Boost", "github.com");
    let work = Account::new("work", "ghe.example.com");
    let work_elsewhere = Account::new("work", "github.com");
    assert_eq!(list.banned(&boost, "lepmoran-boostkh"), Some("bryandlp-boost"));
    assert_eq!(list.banned(&boost, "LEPMORAN-BOOSTKH"), Some("bryandlp-boost"));
    assert_eq!(list.banned(&boost, "other"), Some("bryandlp-boost"));
    assert_eq!(list.banned(&boost, "spammer"), Some("*"));
    assert_eq!(list.banned(&boost, "intern"), None);
    assert_eq!(list.banned(&work, "intern"), Some("GHE.example.com/Work"));
    assert_eq!(list.banned(&work_elsewhere, "intern"), None);
    assert_eq!(list.banned(&work, "lepmoran-boostkh"), None);
    assert_eq!(list.banned(&work, "Spammer"), Some("*"));
    assert_eq!(list.banned(&work, ""), None);
    assert!(list.applies_to(&work_elsewhere));
    assert!(!BanList::from_value(Some(&json!({ "someone": ["x"] }))).applies_to(&work));
    assert_eq!(
        list.rules(),
        [
            (
                "bryandlp-boost".to_string(),
                vec!["Lepmoran-BoostKH".to_string(), "other".to_string()]
            ),
            ("GHE.example.com/Work".to_string(), vec!["intern".to_string()]),
            ("*".to_string(), vec!["spammer".to_string()]),
        ]
    );
    assert!(BanList::from_value(Some(&json!("everyone"))).is_empty());
}

#[test]
fn review_ban_add_and_remove_keep_the_other_keys() {
    let mut ban: Map<String, Value> = serde_json::from_value(json!({ "Bryandlp-Boost": ["a"], "other": 3 })).unwrap();
    assert!(add_ban(&mut ban, "bryandlp-boost", "lepmoran-boostkh"));
    assert!(!add_ban(&mut ban, "BRYANDLP-BOOST", "Lepmoran-Boostkh"));
    assert!(add_ban(&mut ban, "other", "b"));
    assert!(add_ban(&mut ban, "*", "c"));
    assert_eq!(
        Value::Object(ban.clone()),
        json!({ "Bryandlp-Boost": ["a", "lepmoran-boostkh"], "other": ["b"], "*": ["c"] })
    );
    assert!(remove_ban(&mut ban, "bryandlp-boost", "A"));
    assert!(!remove_ban(&mut ban, "bryandlp-boost", "a"));
    assert!(!remove_ban(&mut ban, "nobody", "a"));
    assert!(remove_ban(&mut ban, "other", "b"));
    assert_eq!(
        Value::Object(ban),
        json!({ "Bryandlp-Boost": ["lepmoran-boostkh"], "*": ["c"] })
    );
}

/// A per-account ban: the banning account spends nothing on the pull request (its author came with the search
/// result), the other account still reviews it, and the skip is logged once, not on every poll.
#[test]
fn review_ban_discovery_drops_a_banned_author_for_the_banning_account_only() {
    let context = setup(json!({ "ban": { BOT: [BANNED] } }), None);
    let hub = &context.hub;
    hub.add_account("second", "github.com", false);
    hub.add_pull(by(BANNED, 1, &[BOT, "second"]));
    hub.set_search(&[("review-requested:ultron-bot", &[1]), ("review-requested:second", &[1])]);
    let accounts = vec![context.account.clone(), Account::new("second", "github.com")];
    // Dropped by the poll itself: only the other account's find is queued.
    let (polled, _) = daemon_with(&context, accounts.clone());
    polled.poll();
    assert_eq!(polled.queued(), 1);
    let (instance, outcomes) = daemon_with(&context, accounts);
    for _ in 0..6 {
        instance.once();
    }
    assert_eq!(
        sorted(&outcomes),
        vec![outcome("second", 1, "posted"), outcome("second", 1, "skipped")]
    );
    let users: Vec<String> = hub.with_pull_n(1, |pull| pull.reviews.iter().map(|r| r.user.clone()).collect());
    assert_eq!(users, ["second"]);
    let spent: Vec<String> = hub
        .api("^[A-Z]+ repos/")
        .into_iter()
        .filter(|call| call.env.get("GH_TOKEN") == Some(&context.token))
        .map(|call| format!("{} {}", call.method, call.path))
        .collect();
    assert_eq!(spent, Vec::<String>::new());
    assert_eq!(
        count(
            &context.logs(),
            "github.com/o/r#1 as ultron-bot: skipped (author banned: lepmoran-boostkh)"
        ),
        // Once per daemon: the polled one and the one that ran six cycles.
        2,
        "{}",
        context.logs()
    );
}

/// `*` bans for every account, in any case. Found by a notification (which names no author), the pull request is
/// read once, nothing is posted, and the next poll drops it before reading it again.
#[test]
fn review_ban_star_bans_for_every_account_in_any_case() {
    let context = setup(json!({ "ban": { "*": ["LEPMORAN-BoostKH"] } }), None);
    let hub = &context.hub;
    hub.add_account("second", "github.com", false);
    hub.add_pull(by(BANNED, 1, &[BOT, "second"]));
    hub.lock().notifications = vec![Notification::new("review_requested", 1)];
    let (instance, outcomes) = daemon_with(
        &context,
        vec![context.account.clone(), Account::new("second", "github.com")],
    );
    instance.once();
    assert_eq!(
        sorted(&outcomes),
        vec![outcome("second", 1, "skipped"), outcome("ultron-bot", 1, "skipped")]
    );
    assert_eq!(hub.api("^POST ").len(), 0);
    assert!(context.engine.specs().is_empty());
    let reads = hub.api("^GET repos/o/r/pulls/1$").len();
    assert_eq!(reads, 2);
    instance.once();
    assert_eq!(hub.api("^GET repos/o/r/pulls/1$").len(), reads);
    assert_eq!(outcomes.lock().expect("outcomes").len(), 2);
    let logs = context.logs();
    for login in [BOT, "second"] {
        let line = format!("github.com/o/r#1 as {login}: skipped (author banned: lepmoran-boostkh)");
        assert_eq!(count(&logs, &line), 1, "{logs}");
    }
}

/// A `host/login` key bans for that account on that host only.
#[test]
fn review_ban_host_login_key() {
    let context = setup(json!({ "ban": { "github.com/Ultron-Bot": [BANNED] } }), None);
    context.hub.add_pull(by(BANNED, 1, &[BOT]));
    let skipped = review_pull(&context.deps, &context.requested(), ReviewOptions::default()).expect("decided");
    assert_eq!(skipped, Outcome::Skipped("author banned: lepmoran-boostkh".into()));
    assert!(context.hub.api("^POST ").is_empty());

    let elsewhere = setup(json!({ "ban": { "ghe.example.com/ultron-bot": [BANNED] } }), None);
    elsewhere.hub.add_pull(by(BANNED, 1, &[BOT]));
    let reviewed = review_pull(&elsewhere.deps, &elsewhere.requested(), ReviewOptions::default()).expect("reviewed");
    assert_eq!(reviewed.kind(), "posted");
}

/// `ultron autoreview review <pr>` of a banned author's pull request is refused, naming the ban; nothing is posted.
#[test]
fn review_ban_manual_review_is_refused_naming_the_ban() {
    let context = setup(json!({ "ban": { BOT: [BANNED] } }), None);
    context.hub.add_pull(by(BANNED, 1, &[]));
    let forced = ReviewOptions {
        force: true,
        dry_run: None,
    };
    let error = review_pull(&context.deps, &context.candidate(&["manual"]), forced).expect_err("refused");
    assert_eq!(
        error.to_string(),
        "github.com/o/r#1 as ultron-bot: not reviewed: its author lepmoran-boostkh is banned for ultron-bot (autoreview.ban; \"ultron autoreview unban lepmoran-boostkh --account ultron-bot\" lifts it)"
    );
    let star = setup(json!({ "ban": { "*": [BANNED] } }), None);
    star.hub.add_pull(by(BANNED, 1, &[]));
    let error = review_pull(&star.deps, &star.candidate(&["manual"]), forced).expect_err("refused");
    assert_eq!(
        error.to_string(),
        "github.com/o/r#1 as ultron-bot: not reviewed: its author lepmoran-boostkh is banned for every account (*) (autoreview.ban; \"ultron autoreview unban lepmoran-boostkh\" lifts it)"
    );
    assert!(context.hub.api("^POST ").is_empty() && star.hub.api("^POST ").is_empty());
    assert!(context.engine.specs().is_empty() && star.engine.specs().is_empty());
}

/// A ban added while a pull request waits for a review slot (acknowledged before the ban) drops it before its
/// model turn: no review starts for it and nothing more is posted on it.
#[test]
fn review_ban_added_while_queued_drops_it_before_its_model_turn() {
    let engine = FakeEngine::new(Vec::new());
    let mut context = setup(json!({ "concurrency": 1 }), Some(engine.clone()));
    let bans = live_bans(&mut context);
    let authors = |number: u64| if number == 1 { "alice" } else { BANNED };
    for number in 1..=2 {
        context.hub.add_pull(by(authors(number), number, &[BOT]));
        context
            .hub
            .lock()
            .notifications
            .push(Notification::new("review_requested", number));
    }
    let (gate, starts) = gate(&engine);
    let (instance, outcomes) = daemon_with(&context, vec![context.account.clone()]);
    let store = context.deps.store.clone();
    let worker = std::thread::spawn(move || instance.once());
    starts.recv_timeout(GUARD).expect("a review started");
    wait_until("one review running and one waiting", || {
        let queue = store.read().queue.unwrap_or_default();
        queue.len() == 2 && queue[1].ends_with("(acknowledged, waiting)")
    });
    let waiting: u64 = {
        let queue = store.read().queue.expect("queue");
        let line = queue[1].strip_suffix(" (acknowledged, waiting)").expect("waiting");
        line.rsplit('#').next().expect("number").parse().expect("number")
    };
    let running = 3 - waiting;
    *bans.lock().expect("bans") = ban_all(authors(waiting));
    open(&gate);
    worker.join().expect("once");
    assert_eq!(sorted(&outcomes), {
        let mut expected = vec![outcome(BOT, running, "posted"), outcome(BOT, waiting, "skipped")];
        expected.sort();
        expected
    });
    let specs = engine.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0]["label"], format!("o/r#{running}"));
    assert_eq!(
        context
            .hub
            .api(&format!("^POST repos/o/r/pulls/{waiting}/reviews$"))
            .len(),
        0
    );
    assert_eq!(
        context
            .hub
            .api(&format!("^POST repos/o/r/issues/{waiting}/comments$"))
            .len(),
        1
    );
    let line = format!(
        "github.com/o/r#{waiting} as ultron-bot: skipped (author banned: {})",
        authors(waiting)
    );
    assert_eq!(count(&context.logs(), &line), 1, "{}", context.logs());
}

/// A ban added during the model turn: the finished review is discarded, nothing is posted, no attempt is counted.
#[test]
fn review_ban_added_during_the_model_turn_posts_nothing() {
    let engine = FakeEngine::new(vec![Ok(engine_result(json!({ "findings": [major()] })))]);
    let mut context = setup(json!({}), Some(engine.clone()));
    let bans = live_bans(&mut context);
    context.hub.add_pull(by(BANNED, 1, &[BOT]));
    let during = bans.clone();
    *engine.on_review.lock().expect("hook") = Some(Box::new(move || {
        *during.lock().expect("bans") = ban_all(BANNED);
    }));
    let outcome = review_pull(&context.deps, &context.requested(), ReviewOptions::default()).expect("reviewed");
    assert_eq!(outcome, Outcome::Skipped("author banned: lepmoran-boostkh".into()));
    assert_eq!(engine.specs().len(), 1);
    assert!(context.hub.api("^POST repos/o/r/pulls/1/reviews$").is_empty());
    let state = context.pull_state();
    assert!(
        state.attempts.is_empty() && state.last_reviewed_sha.is_none(),
        "{state:?}"
    );
    assert!(context.logs().contains(
        "github.com/o/r#1 as ultron-bot: author banned: lepmoran-boostkh; the finished review was discarded"
    ));
}

/// A banned login's review request or mention on someone else's pull request asks for nothing (who requested is the
/// issue's `review_requested` event's actor); somebody else's mention still gets it reviewed.
#[test]
fn review_ban_request_or_mention_by_a_banned_login_is_ignored() {
    let context = setup(json!({ "ban": { BOT: [BANNED] } }), None);
    let hub = &context.hub;
    let mut pull = by("alice", 1, &[BOT]);
    pull.events = vec![
        json!({ "event": "review_requested", "actor": { "login": "Lepmoran-BoostKH" },
        "requested_reviewer": { "login": BOT }, "created_at": "2026-10-04T09:00:00Z" }),
    ];
    pull.issue_comments = vec![FakeComment::new(
        1,
        BANNED,
        &format!("@{BOT} review this"),
        "2026-10-04T09:10:00Z",
    )];
    hub.add_pull(pull);
    let skipped = review_pull(&context.deps, &context.requested(), ReviewOptions::default()).expect("decided");
    assert_eq!(
        skipped,
        Outcome::Skipped(
            "no review request or mention for this account; ignored the review request by Lepmoran-BoostKH and a mention by a banned login (autoreview.ban)"
                .into()
        )
    );
    assert!(hub.api("^POST ").is_empty());
    // Requested by someone else, the review is due.
    let requested = setup(json!({ "ban": { BOT: [BANNED] } }), None);
    let mut asked = by("alice", 1, &[BOT]);
    asked.events = vec![json!({ "event": "review_requested", "actor": { "login": "bob" },
        "requested_reviewer": { "login": BOT }, "created_at": "2026-10-04T09:00:00Z" })];
    requested.hub.add_pull(asked);
    let reviewed = review_pull(&requested.deps, &requested.requested(), ReviewOptions::default()).expect("reviewed");
    assert_eq!(reviewed.kind(), "posted");
    // Only a banned login's mention: ignored; another person's mention: reviewed.
    let mentioned = setup(json!({ "ban": { BOT: [BANNED] } }), None);
    let mut quiet = by("alice", 1, &[]);
    quiet.issue_comments = vec![FakeComment::new(
        1,
        BANNED,
        &format!("@{BOT} look"),
        "2026-10-04T09:10:00Z",
    )];
    mentioned.hub.add_pull(quiet);
    let skipped = review_pull(
        &mentioned.deps,
        &mentioned.candidate(&["mention"]),
        ReviewOptions::default(),
    )
    .expect("decided");
    assert_eq!(
        skipped,
        Outcome::Skipped(
            "no review request or mention for this account; ignored a mention by a banned login (autoreview.ban)"
                .into()
        )
    );
    mentioned.hub.with_pull(|pull| {
        pull.issue_comments.push(FakeComment::new(
            2,
            "carol",
            &format!("@{BOT} please"),
            "2026-10-04T09:20:00Z",
        ));
    });
    let reviewed = review_pull(
        &mentioned.deps,
        &mentioned.candidate(&["mention"]),
        ReviewOptions::default(),
    )
    .expect("reviewed");
    assert_eq!(reviewed.kind(), "posted");
}

/// A list that cannot be read (the settings file mid-write) keeps the last one in force: it lifts no ban.
#[test]
fn review_ban_unreadable_list_keeps_the_last_one() {
    let reads = Arc::new(Mutex::new(vec![None, Some(ban_all(BANNED)), Some(BanList::default())]));
    let next = reads.clone();
    let bans = Bans::reloading(Arc::new(move || next.lock().expect("reads").pop().flatten()));
    let account = Account::new(BOT, "github.com");
    assert!(bans.get().is_empty());
    assert_eq!(bans.refresh().banned(&account, BANNED), Some("*"));
    assert_eq!(bans.refresh().banned(&account, BANNED), Some("*"));
    assert_eq!(bans.get().banned(&account, BANNED), Some("*"));
}
