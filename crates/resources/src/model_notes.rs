//! Notes for the model a prompt is rendered for: short rules a model family needs said outright, which the others
//! follow unasked (or are hurt by). One table, matched by the start of the model id.

const COMMENTS: &str = "Comments: write one only where the code cannot show its reason (a hidden constraint, an invariant, a workaround for a named bug). Never describe what the code does, and never mention this task, its review or its callers in a comment.";
const PROSE: &str = "Prose for the user: plain words in whole sentences. No clipped shorthand, no arrows or symbols in place of words, no ornate phrasing.";
const PARALLEL: &str =
    "Independent operations (reads and searches above all) go in one cell under `asyncio.gather`, not one cell each.";

/// Model id prefix (dots read as dashes) and its notes; the first matching row wins, so a longer prefix comes first.
const TABLE: &[(&str, &[&str])] = &[
    ("claude-fable-5-", &[COMMENTS, PARALLEL, PROSE]),
    ("claude-fable-5", &[COMMENTS, PROSE]),
    ("claude-opus-5", &[COMMENTS, PROSE]),
    ("claude-sonnet-5", &[COMMENTS]),
    ("claude-opus-4-7", &[COMMENTS]),
    ("claude-opus-4-8", &[COMMENTS]),
];

/// The notes for `model_id` as one prompt section's text, `None` for a model without any.
pub fn model_notes(model_id: &str) -> Option<String> {
    let id = model_id.replace('.', "-");
    TABLE
        .iter()
        .find(|(prefix, _)| id.starts_with(prefix))
        .map(|(_, notes)| {
            notes
                .iter()
                .map(|note| format!("- {note}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
}
