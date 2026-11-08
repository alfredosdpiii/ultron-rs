//! The per-review record: every posted review leaves one, and only the newest are kept.

use crate::github_common;

use std::time::{Duration, SystemTime};

use github_common::*;
use review::reviewer::{MAX_REVIEW_RECORDS, Outcome, ReviewOptions, review_pull};
use serde_json::{Value, json};

#[test]
fn review_records_keep_the_newest_and_drop_the_oldest() {
    let context = setup(
        json!({}),
        Some(FakeEngine::new(vec![Ok(engine_result(
            json!({ "findings": [major()] }),
        ))])),
    );
    context.hub.add_pull(FakePull::new("o", "r", 1).requested(&[BOT]));
    let reviews = std::path::Path::new(&context.deps.paths.dir).join("reviews");
    std::fs::create_dir_all(&reviews).expect("reviews dir");
    let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
    for n in 0..MAX_REVIEW_RECORDS {
        let file = std::fs::File::create(reviews.join(format!("earlier-{n:04}.json"))).expect("earlier record");
        file.set_modified(an_hour_ago + Duration::from_secs(n as u64))
            .expect("mtime");
    }

    let outcome = review_pull(&context.deps, &context.requested(), ReviewOptions::default()).expect("review");
    assert!(matches!(outcome, Outcome::Posted(_)), "{outcome:?}");

    let mut names: Vec<String> = std::fs::read_dir(&reviews)
        .expect("listed")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), MAX_REVIEW_RECORDS);
    assert_eq!(names[0], "earlier-0001.json");
    let name = format!("o-r-1-{}-{BOT}.json", &head()[..7]);
    assert!(names.contains(&name), "{names:?}");
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(reviews.join(&name)).expect("read")).expect("json");
    assert_eq!(record["review"]["event"], "REQUEST_CHANGES", "{record}");
}
