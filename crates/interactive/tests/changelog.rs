//! `/changelog`: replays fixtures/changelog.json, recorded from utils/changelog.ts by fixtures/changelog.mjs.

use interactive::changelog::{changelog_tokens, normalize_links, parse};
use serde_json::Value;
use tui::markdown_tokens;

/// What `/changelog` shows, as one document: the entries oldest first, links pinned, or Pi's empty notice. The
/// oracle `changelog_tokens` must agree with when the document is lexed whole.
fn changelog_markdown(content: Option<&str>) -> String {
    let mut entries = content.map(parse).unwrap_or_default();
    if entries.is_empty() {
        return "No changelog entries found.".into();
    }
    entries.reverse();
    entries
        .iter()
        .map(|e| normalize_links(&e.content, e.version))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/changelog.json")).unwrap()
}

#[test]
fn interactive_changelog_links_pin_to_the_release_like_ts() {
    for sample in fixture()["samples"].as_array().unwrap() {
        assert_eq!(
            normalize_links(sample["markdown"].as_str().unwrap(), (1, 2, 3)),
            sample["normalized"].as_str().unwrap()
        );
    }
}

#[test]
fn interactive_changelog_entries_match_ts() {
    let content = include_str!("../../../packages/coding-agent/CHANGELOG.md");
    let fixture = fixture();
    // The released versions and the newest entries pin the parse; the recorded `content` length is not compared,
    // since every edit under [Unreleased] changes it and the recording script is gone.
    let entries = parse(content);
    let versions: Vec<String> = entries
        .iter()
        .map(|e| format!("{}.{}.{}", e.version.0, e.version.1, e.version.2))
        .collect();
    let want: Vec<String> = fixture["changelog"]["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(versions, want);
    for (entry, want) in entries.iter().zip(fixture["changelog"]["newest"].as_array().unwrap()) {
        assert_eq!(normalize_links(&entry.content, entry.version), want.as_str().unwrap());
    }
}

#[test]
fn interactive_changelog_oldest_first_and_empty_notice() {
    let text = "# Changelog\n\n## [Unreleased]\n\nx\n\n## [0.2.0] - d\n\nnew\n\n## 0.1.0\n\nold [a](docs/a.md)\n";
    assert_eq!(
        changelog_tokens(Some(text)),
        markdown_tokens(
            "## 0.1.0\n\nold [a](https://github.com/earendil-works/pi/blob/v0.1.0/packages/coding-agent/docs/a.md)\n\n## [0.2.0] - d\n\nnew"
        )
    );
    let empty = markdown_tokens("No changelog entries found.");
    assert_eq!(changelog_tokens(None), empty);
    assert_eq!(changelog_tokens(Some("no versions")), empty);
}

#[test]
fn interactive_changelog_lexed_per_entry_matches_the_whole_document() {
    // The newest 20 releases of the real CHANGELOG.md (lists, code, links, nested items), lexed entry by entry on
    // several threads, give the tokens of the joined document that MarkdownNotice lexed.
    let content = include_str!("../../../packages/coding-agent/CHANGELOG.md");
    let mut starts = content.match_indices("\n## [").map(|(i, _)| i + 1);
    let end = starts.nth(21).unwrap();
    let sample = &content[..end];
    assert_eq!(parse(sample).len(), 20);
    assert_eq!(
        changelog_tokens(Some(sample)),
        markdown_tokens(changelog_markdown(Some(sample)).trim())
    );
}
