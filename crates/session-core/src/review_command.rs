//! `/review` (packages/coding-agent/src/ultron/review.ts): the typed command becomes a short prompt that asks the
//! model to run Ultron's built-in review in one REPL cell and relay its report. The review itself is Python
//! (`review_api`); nothing about it is in the system prompt of ordinary turns.
//!
//! The session expands it where every prompt and queued message enters it (agent-controller-provider.ts
//! `toTextPrompt`: `prompt`, `steer`, `followUp` and `nextRun`; `Core::user_text`), so the TUI, queued messages,
//! `ultron -p "/review"` and RPC clients behave the same. Clients send `/review <args>` as typed.

/// `reviewPrompt`: the model-facing prompt for `/review <args>`.
pub fn review_prompt(args: &str) -> String {
    let trimmed = jsrt::string::trim(args);
    // A JSON string literal is a valid Python string literal for any argument text.
    let literal = serde_json::to_string(trimmed).unwrap_or_else(|_| "\"\"".into());
    let mut lines = vec![
        if trimmed.is_empty() {
            "/review".to_string()
        } else {
            format!("/review {trimmed}")
        },
        String::new(),
        "Run Ultron's built-in code review in one REPL cell, exactly:".into(),
        String::new(),
        "import review_api".into(),
        format!("review = await review_api.run(rlm, {literal})"),
        "print(review.report)".into(),
        String::new(),
        "Then reply with the printed report as it is. Do not review the code yourself, edit files, or run the review again unless the cell raised an error.".into(),
    ];
    if posts(trimmed) {
        lines.push(String::new());
        lines.push("If the report says a post is pending, ask me whether to post it to the pull request. Post only after I answer yes in a later message, with `await review_api.post(review, confirm=True)`; without that answer, never post.".into());
    }
    lines.join("\n")
}

/// `/(^|\s)--post(\s|=|$)/`.
fn posts(args: &str) -> bool {
    args.match_indices("--post").any(|(at, flag)| {
        let before = args[..at].chars().next_back();
        let after = args[at + flag.len()..].chars().next();
        before.is_none_or(jsrt::string::is_space) && after.is_none_or(|c| jsrt::string::is_space(c) || c == '=')
    })
}

/// `expandReviewCommand`: `/review [args]` (after trimming; `/^\/review(?:\s+([\s\S]*))?$/`) becomes the review
/// prompt; any other text is returned unchanged.
pub fn expand_review_command(text: &str) -> String {
    let Some(rest) = jsrt::string::trim(text).strip_prefix("/review") else {
        return text.to_string();
    };
    if rest.is_empty() {
        return review_prompt("");
    }
    if rest.starts_with(jsrt::string::is_space) {
        return review_prompt(rest);
    }
    text.to_string()
}
