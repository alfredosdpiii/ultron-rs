//! `/learning`: Socratic + Ultralearning tutoring mode backed by `/skill:learning` and `learning_api.py`.
//!
//! Expands where every prompt enters the session (`Core::user_text`), so the TUI, `ultron -p "/learning ..."`,
//! and RPC clients behave identically without adding tokens to normal turns.

const OFF_PROMPT: &str = concat!(
    "/learning off\n\n",
    "Exit learning mode and return to normal Ultron assistance. ",
    "If a topic was active in `learning_api`, confirm its state is saved.",
);

const LIST_PROMPT: &str = concat!(
    "/learning list\n\n",
    "Run one REPL cell:\n\n",
    "import learning_api\n",
    "for item in learning_api.list_topics():\n",
    "    print(f\"- {item['topic']} ({item['mastery_pct']}%, updated {item['updated_at']})\")\n\n",
    "If empty, say no saved learning topics exist yet and ask what the user wants to learn.",
);

const EMPTY_PROMPT: &str = concat!(
    "/learning\n\n",
    "Enter `/learning` mode (read `/skill:learning`). ",
    "Check `learning_api.list_topics()` in one REPL cell; if saved topics exist, ",
    "list them briefly and ask whether to resume one or start a new topic. ",
    "Ask ONE short question and wait.",
);

fn topic_hint(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "the active topic (or most recent topic from `learning_api.list_topics()`)".into()
    } else {
        format!("topic `{trimmed}`")
    }
}

/// Model-facing prompt for `/learning <args>`.
pub fn learning_prompt(args: &str) -> String {
    let trimmed = jsrt::string::trim(args);
    let lowered = trimmed.to_ascii_lowercase();
    if matches!(lowered.as_str(), "off" | "exit" | "stop") {
        return OFF_PROMPT.into();
    }
    if lowered == "list" {
        return LIST_PROMPT.into();
    }
    if lowered == "progress" || lowered.starts_with("progress ") {
        let hint = topic_hint(trimmed.get(8..).unwrap_or(""));
        return format!("/learning {trimmed}\n\nRender the learning dashboard for {hint} in one REPL cell ",)
            + "with `print(learning_api.render_dashboard(topic))` and relay it verbatim.";
    }
    if lowered == "graph" || lowered.starts_with("graph ") {
        let hint = topic_hint(trimmed.get(5..).unwrap_or(""));
        return format!("/learning {trimmed}\n\nFor {hint}, run one REPL cell that prints ")
            + "`learning_api.render_chain_table(state)` and renders "
            + "`await diagram.mermaid(learning_api.mermaid_graph(state))` inline in the TUI.";
    }
    if lowered == "practice" || lowered.starts_with("practice ") {
        let topic = trimmed.get(8..).map_or("", str::trim);
        return format!("/learning {trimmed}\n\nEnter `/learning` Practice mode using `/skill:learning` ")
            + &format!("and `learning_api`. Load `{topic}` (or the most recent topic), pick a due review ")
            + "or stored proposition, and pose ONE varied retrieval problem WITHOUT naming the "
            + "underlying rule up front. Ask for reasoning and confidence (0/30/50/70/90/100%).";
    }
    if lowered.starts_with("drill ") {
        let subskill = trimmed.get(6..).map_or("", str::trim);
        return format!("/learning {trimmed}\n\nEnter Ultralearning Drill mode using ")
            + &format!("`/skill:learning` and `learning_api` on subskill `{subskill}`. ")
            + "Pose ONE concrete, high-ICAP exercise targeting this bottleneck and wait.";
    }
    if trimmed.is_empty() {
        return EMPTY_PROMPT.into();
    }
    let literal = serde_json::to_string(trimmed).unwrap_or_else(|_| "\"\"".into());
    format!(
        "/learning {trimmed}\n\nEnter `/learning` mode for topic: {trimmed}\n\n\
Follow `/skill:learning` strictly (Socratic Tutor + LearnHarness Probe->Teach, \
6-stop Confidence Rail, `A ∧ B ⟹ C` Proposition Graph + Ultralearning + active retrieval):\n\
1. Check `learning_api.load({literal})` in one REPL cell; if found, print \
`learning_api.render_dashboard({literal})` and resume from the bottleneck or due review.\n\
2. Otherwise start Phase 1 (Probe): split into 4-10 subskills + Concepts/Facts/Procedures, \
say you will ask 3-5 diagnostic questions before teaching, and ask ONLY Question 1 now."
    )
}

/// Expands `/learning [args]` into the learning-mode prompt; any other text is returned unchanged.
pub fn expand_learning_command(text: &str) -> String {
    let Some(rest) = jsrt::string::trim(text).strip_prefix("/learning") else {
        return text.to_string();
    };
    if rest.is_empty() {
        return learning_prompt("");
    }
    if rest.starts_with(jsrt::string::is_space) {
        return learning_prompt(rest);
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_learning_subcommands() {
        assert_eq!(expand_learning_command("hello"), "hello");
        assert_eq!(expand_learning_command("/learningfoo"), "/learningfoo");
        assert!(expand_learning_command("/learning").contains("learning_api.list_topics()"));
        assert!(expand_learning_command("/learning rust lifetimes").contains("Phase 1 (Probe)"));
        assert!(expand_learning_command("/learning progress").contains("render_dashboard"));
        assert!(expand_learning_command("/learning graph").contains("diagram.mermaid"));
        assert!(expand_learning_command("/learning practice").contains("WITHOUT naming"));
        assert!(expand_learning_command("/learning drill borrow-checker").contains("Ultralearning Drill"));
        assert!(expand_learning_command("/learning off").contains("Exit learning mode"));
    }
}
