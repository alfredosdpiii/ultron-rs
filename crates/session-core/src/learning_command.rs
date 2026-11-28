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

fn parse_start_mode(trimmed: &str) -> (Option<&'static str>, &str) {
    for (flag, mode) in [
        ("--beginner ", "beginner"),
        ("--hands-on ", "hands-on"),
        ("--probe ", "probe"),
        ("--advanced ", "advanced"),
    ] {
        if let Some(rest) = trimmed.strip_prefix(flag) {
            return (Some(mode), rest.trim());
        }
    }
    (None, trimmed)
}

const BEGINNER_ONBOARDING: &str = concat!(
    "2. Otherwise start Phase 1A (Teach-First Beginner Onboarding — DO NOT quiz yet): ",
    "narrow broad topics into 4-6 approachable milestones and save via ",
    "`learning_api.start(...)` at level 0. Give a concise plain-English mental model + ",
    "roadmap, then teach Milestone 1 clearly with a concrete analogy and tiny example. ",
    "End with ONE gentle check-for-understanding question strictly on what you just ",
    "explained to begin the Socratic loop.",
);

const HANDS_ON_ONBOARDING: &str = concat!(
    "2. Otherwise start Phase 1A (Hands-On First — DO NOT quiz yet): narrow the topic ",
    "into 4-6 milestones and save via `learning_api.start(...)` at level 0. Show a brief ",
    "roadmap, then walk through (and run in the REPL when possible) one tiny concrete ",
    "example explaining what each piece does. End with ONE small prediction or ",
    "modification question on that example.",
);

const PROBE_ONBOARDING: &str = concat!(
    "2. Otherwise start Phase 1B (Diagnostic Probe): split into 4-10 subskills + ",
    "Concepts/Facts/Procedures, say you will ask 3-5 diagnostic questions before ",
    "teaching, and ask ONLY Question 1 now.",
);

const ADVANCED_ONBOARDING: &str = concat!(
    "2. Otherwise start Phase 1B (Advanced Diagnostic): skip introductory basics, split ",
    "into 4-10 subskills focusing on internals, invariants, and edge cases, say you will ",
    "ask 3-5 targeted diagnostic questions, and ask ONLY Question 1 now.",
);

const DEFAULT_ONBOARDING: &str = concat!(
    "2. Otherwise DO NOT assume prior knowledge or start quizzing yet: ask ONE short ",
    "question asking whether the learner is brand new (teach me first from scratch with ",
    "a roadmap & example) or already has background (quick 3-5 question diagnostic to ",
    "find their edge), and wait for their reply.",
);

fn new_topic_prompt(trimmed: &str) -> String {
    let (mode, topic) = parse_start_mode(trimmed);
    let literal = serde_json::to_string(topic).unwrap_or_else(|_| "\"\"".into());
    let onboarding = match mode {
        Some("beginner") => BEGINNER_ONBOARDING,
        Some("hands-on") => HANDS_ON_ONBOARDING,
        Some("probe") => PROBE_ONBOARDING,
        Some("advanced") => ADVANCED_ONBOARDING,
        _ => DEFAULT_ONBOARDING,
    };
    format!(
        "/learning {trimmed}\n\nEnter `/learning` mode for topic: {topic}\n\n\
Follow `/skill:learning` strictly (Worked-Example Teach-First for beginners, Socratic Tutor + \
LearnHarness Probe->Teach, 6-stop Confidence Rail, `A ∧ B ⟹ C` Proposition Graph + Ultralearning):\n\
1. Check `learning_api.load({literal})` in one REPL cell; if found, print \
`learning_api.render_dashboard({literal})` and resume from the bottleneck or due review.\n\
{onboarding}"
    )
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
        let topic = trimmed.get(8..).map_or("", str::trim);
        let call = if topic.is_empty() {
            "learning_api.render_dashboard()".to_string()
        } else {
            let lit = serde_json::to_string(topic).unwrap_or_else(|_| "\"\"".into());
            format!("learning_api.render_dashboard({lit})")
        };
        return format!("/learning {trimmed}\n\nRun one REPL cell: `import learning_api; print({call})` ")
            + "and relay the dashboard verbatim.";
    }
    if lowered.starts_with("switch ") {
        let topic = trimmed.get(7..).map_or("", str::trim);
        let lit = serde_json::to_string(topic).unwrap_or_else(|_| "\"\"".into());
        return format!(
            "/learning {trimmed}\n\nSwitch active learning topic to `{topic}` in one REPL cell:\n\n\
import learning_api\nlearning_api.switch({lit})\nprint(learning_api.render_dashboard({lit}))\n\n\
Then resume `/skill:learning` from the bottleneck or due review and ask ONE question."
        );
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
    new_topic_prompt(trimmed)
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
        let cases = [
            ("/learning", "learning_api.list_topics()"),
            ("/learning cuda programming", "DO NOT assume prior knowledge"),
            ("/learning --beginner cuda programming", "Teach-First Beginner"),
            ("/learning --hands-on cuda programming", "Hands-On First"),
            ("/learning --probe rust lifetimes", "Phase 1B (Diagnostic Probe)"),
            ("/learning --advanced rust lifetimes", "Advanced Diagnostic"),
            ("/learning progress", "render_dashboard"),
            ("/learning switch rust", "learning_api.switch"),
            ("/learning graph", "diagram.mermaid"),
            ("/learning practice", "WITHOUT naming"),
            ("/learning drill borrow-checker", "Ultralearning Drill"),
            ("/learning off", "Exit learning mode"),
        ];
        for (input, expected) in cases {
            assert!(expand_learning_command(input).contains(expected), "{input}");
        }
    }
}
