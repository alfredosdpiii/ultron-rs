//! The check library: plain-code checks over an engine result (what a review record keeps under `result`), seeded
//! with Hrishi Olickel's ten detection checks ("FMEA report: detection playbook"), each read for a reviewer. A check
//! names a way a review goes wrong; its hits are failure modes in the register ([`crate::register`]). A check
//! counts only after it has caught a planted bug and stayed quiet on the same result without it
//! ([`crate::eval::prove`], the playbook's eighth rule).

use serde_json::Value;

pub struct Check {
    pub id: &'static str,
    /// The playbook's rule it comes from.
    pub rule: &'static str,
    /// What it flags in a review.
    pub flags: &'static str,
    /// The hits of one engine result, one line each.
    pub detect: fn(&Value) -> Vec<String>,
}

fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn number(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn place(finding: &Value) -> String {
    format!("{}:{}", text(finding, "file"), number(finding, "line"))
}

fn confirmed(result: &Value) -> impl Iterator<Item = &Value> {
    items(result, "findings")
        .iter()
        .filter(|finding| text(finding, "verification") == "confirmed")
}

/// 1. A confirmed finding that quotes nothing of the source it is about.
fn unquoted(result: &Value) -> Vec<String> {
    confirmed(result)
        .filter(|finding| text(finding, "evidence").trim().is_empty())
        .map(|finding| format!("{} was confirmed without quoting the source", place(finding)))
        .collect()
}

/// 2. More added lines than the loaded hunks hold.
fn lost_lines(result: &Value) -> Vec<String> {
    let loaded: i64 = result
        .get("diffLines")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|files| files.values())
        .flat_map(|ranges| ranges.as_array().into_iter().flatten())
        .filter_map(|range| {
            let pair = range.as_array()?;
            Some(pair.get(1)?.as_i64()? - pair.first()?.as_i64()? + 1)
        })
        .sum();
    let added = number(result, "added");
    (added > loaded)
        .then(|| format!("{added} lines were added, but the loaded hunks hold {loaded}"))
        .into_iter()
        .collect()
}

/// 3. Usage totals that are not the sum of their phases.
fn unsummed(result: &Value) -> Vec<String> {
    let usage = result.get("usage").cloned().unwrap_or(Value::Null);
    let phases: Vec<&Value> = usage
        .get("byPhase")
        .and_then(Value::as_object)
        .map(|phases| phases.values().collect())
        .unwrap_or_default();
    if phases.is_empty() {
        return Vec::new();
    }
    ["frames", "tokens"]
        .into_iter()
        .filter_map(|key| {
            let total = number(&usage, key);
            let parts: i64 = phases.iter().map(|phase| number(phase, key)).sum();
            (total != parts).then(|| format!("usage.{key} is {total}, its phases sum to {parts}"))
        })
        .collect()
}

/// 4. Duplicates dropped without a record of what each was merged into.
fn unrecorded_merges(result: &Value) -> Vec<String> {
    let dropped = result.get("dropped").cloned().unwrap_or(Value::Null);
    let (count, recorded) = (
        number(&dropped, "duplicates"),
        items(&dropped, "duplicateOf").len() as i64,
    );
    (count > recorded)
        .then(|| format!("{count} duplicates were dropped, {recorded} recorded with what they merged into"))
        .into_iter()
        .collect()
}

/// 5. A complete review with nothing confirmed while the deep pass left checks open.
fn green_with_open_checks(result: &Value) -> Vec<String> {
    if result.get("complete") != Some(&Value::Bool(true)) || confirmed(result).next().is_some() {
        return Vec::new();
    }
    items(result, "notChecked")
        .iter()
        .filter_map(Value::as_str)
        .filter(|line| line.starts_with("Not traced by the deep pass") || line.starts_with("Checks the deep pass"))
        .map(|line| format!("nothing was found while: {line}"))
        .collect()
}

/// 6. Findings left unverified because the budget or the deadline ran out.
fn spent_early(result: &Value) -> Vec<String> {
    let mut hits: Vec<String> = items(result, "findings")
        .iter()
        .filter(|finding| text(finding, "note").starts_with("not verified (budget)"))
        .map(|finding| format!("{} was not verified within the budget", place(finding)))
        .collect();
    hits.extend(
        items(result, "incomplete")
            .iter()
            .filter_map(Value::as_str)
            .filter(|line| line.contains("token budget") || line.contains("deadline"))
            .map(str::to_string),
    );
    hits
}

/// 7. A suggested replacement that silences a test instead of fixing the code.
fn shortcut(result: &Value) -> Vec<String> {
    let skip = regex::Regex::new(
        r"@pytest\.mark\.(skip|xfail)|#\[ignore\]|\b(it|test|describe)\.skip\(|\bxit\(|unittest\.skip|\bt\.Skip\(",
    )
    .expect("static pattern");
    items(result, "findings")
        .iter()
        .filter(|finding| skip.is_match(text(finding, "replacement")))
        .map(|finding| format!("{} suggests skipping a test", place(finding)))
        .collect()
}

/// 8. A deterministic check that reported clean having checked no file: a check that cannot fail.
fn unfailable(result: &Value) -> Vec<String> {
    let stages = result
        .get("static")
        .map_or(&[][..], |static_out| items(static_out, "stages"));
    stages
        .iter()
        .filter(|stage| text(stage, "status") == "clean" && items(stage, "files").is_empty())
        .map(|stage| format!("the {} check reported clean on no file", text(stage, "name")))
        .collect()
}

/// 9. A finding whose two scales disagree (its level and its old-scale severity).
fn scales_disagree(result: &Value) -> Vec<String> {
    let old = |level: &str| match level {
        "critical" => "blocker",
        "high" => "major",
        "medium" | "low" => "minor",
        "nit" => "nit",
        _ => "",
    };
    items(result, "findings")
        .iter()
        .filter(|finding| {
            let level = text(finding, "level");
            !level.is_empty() && old(level) != text(finding, "severity")
        })
        .map(|finding| {
            format!(
                "{} is {} but its severity says {}",
                place(finding),
                text(finding, "level"),
                text(finding, "severity")
            )
        })
        .collect()
}

/// 10. The same tests run twice at one commit with different outcomes.
fn flipped(result: &Value) -> Vec<String> {
    let runs = result.get("tests").map_or(&[][..], |tests| items(tests, "runs"));
    let mut seen: Vec<(String, &str)> = Vec::new();
    let mut hits = Vec::new();
    for run in runs {
        let key = format!(
            "{} {} {}",
            text(run, "kind"),
            text(run, "rev"),
            items(run, "paths")
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        );
        let status = text(run, "status");
        match seen.iter().find(|(seen, _)| *seen == key) {
            Some((_, first)) if *first != status && !hits.iter().any(|hit: &String| hit.starts_with(&key)) => {
                hits.push(format!("{key} ran {first} and then {status}"));
            }
            Some(_) => {}
            None => seen.push((key, status)),
        }
    }
    hits
}

/// The library, seeded with the playbook's ten (in its order).
pub const LIBRARY: [Check; 10] = [
    Check {
        id: "look-first",
        rule: "Look before you transform",
        flags: "a confirmed finding that quotes nothing of the source",
        detect: unquoted,
    },
    Check {
        id: "before-after",
        rule: "Compare before and after loading",
        flags: "more added lines than the loaded hunks hold",
        detect: lost_lines,
    },
    Check {
        id: "recompute",
        rule: "Recompute every number you report",
        flags: "usage totals that are not the sum of their phases",
        detect: unsummed,
    },
    Check {
        id: "undoable",
        rule: "Prove a lossy step can be undone",
        flags: "duplicates dropped without a record of what they merged into",
        detect: unrecorded_merges,
    },
    Check {
        id: "green-not-done",
        rule: "A passing test isn't the finish",
        flags: "a clean review while the deep pass left checks open",
        detect: green_with_open_checks,
    },
    Check {
        id: "budget-end",
        rule: "Save budget for the end",
        flags: "findings left unverified when the budget or deadline ran out",
        detect: spent_early,
    },
    Check {
        id: "remove-path",
        rule: "Take access away, don't just ask",
        flags: "a suggested fix that skips a test",
        detect: shortcut,
    },
    Check {
        id: "fail-once",
        rule: "Make every check fail once",
        flags: "a deterministic check that reported clean having checked nothing",
        detect: unfailable,
    },
    Check {
        id: "conventions",
        rule: "Agree on conventions first",
        flags: "a finding whose level and severity disagree",
        detect: scales_disagree,
    },
    Check {
        id: "run-twice",
        rule: "Run it more than once",
        flags: "tests whose outcome flipped between runs of one commit",
        detect: flipped,
    },
];
