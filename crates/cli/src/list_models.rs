//! `listModels` from packages/coding-agent/src/cli/list-models.ts over the built-in catalog and models.json.

use ai::Model;
use std::path::Path;

/// The warning `--list-models` writes to stderr when models.json did not load or a provider in it does not compose.
pub fn models_json_warning(env: &crate::Env) -> Option<String> {
    session_core::lane::registry(env)
        .error()
        .map(|error| format!("{}\n", ai::model_config::models_json_warning(&error)))
}

/// `formatTokenCount`: 200000 -> "200K", 1000000 -> "1M", 131072 -> "131.1K", 1150 -> "1.1K" (`toFixed(1)` of the
/// double `1.15`, which lies just below the tie).
pub fn format_token_count(count: u64) -> String {
    for (unit, suffix) in [(1_000_000, "M"), (1_000, "K")] {
        if count >= unit {
            if count.is_multiple_of(unit) {
                return format!("{}{suffix}", count / unit);
            }
            return format!("{}{suffix}", jsrt::number::to_fixed(count as f64 / unit as f64, 1));
        }
    }
    count.to_string()
}

use jsrt::collate::locale_compare;

/// The `--list-models` output for `models`, filtered by `search`.
pub fn render(models: &[Model], search: Option<&str>, docs: &Path) -> String {
    if models.is_empty() {
        return format!(
            "No models available. Use /login to log into a provider via OAuth or API key. See:\n  {}\n  {}\n",
            docs.join("providers.md").display(),
            docs.join("models.md").display()
        );
    }
    let all: Vec<&Model> = models.iter().collect();
    let mut filtered: Vec<&Model> = match search {
        Some(q) if !q.is_empty() => tui::fuzzy_filter(&all, q, |m| format!("{} {}", m.provider, m.id)),
        _ => all,
    };
    if filtered.is_empty() {
        return format!("No models matching \"{}\"\n", search.unwrap_or_default());
    }
    filtered.sort_by(|a, b| locale_compare(&a.provider, &b.provider).then_with(|| locale_compare(&a.id, &b.id)));
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    let mut rows = vec![["provider", "model", "context", "max-out", "thinking", "images"].map(String::from)];
    rows.extend(filtered.iter().map(|m| {
        [
            m.provider.clone(),
            m.id.clone(),
            format_token_count(m.context_window),
            format_token_count(m.max_tokens),
            yes_no(m.reasoning).into(),
            yes_no(m.input.iter().any(|i| i == "image")).into(),
        ]
    }));
    let widths: Vec<usize> = (0..6)
        .map(|c| rows.iter().map(|r| r[c].encode_utf16().count()).max().unwrap_or(0))
        .collect();
    let mut text = String::new();
    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell}{}", " ".repeat(w - cell.encode_utf16().count())))
            .collect();
        text.push_str(&cells.join("  "));
        text.push('\n');
    }
    text
}
