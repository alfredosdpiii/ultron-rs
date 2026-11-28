//! The session report as text (`ultron/session-report-text.ts`): the full report of one session and the
//! one-line-per-session table. A number the session did not keep prints as `not recorded` (`n/r` in the table), and
//! a cost nobody reported as `unknown`: neither is ever printed as zero. Plain style only (the CLI prints no color).

use jsrt::localtime::Zone;
use serde_json::{Map, Value};

const NOT_RECORDED: &str = "not recorded";

pub struct TextOptions<'a> {
    pub utc: bool,
    pub home: &'a str,
    pub zone: &'a Zone,
}

pub fn format_tokens(tokens: f64) -> String {
    if tokens < 1000.0 {
        return jsrt::number::to_string(tokens);
    }
    if tokens < 999_950.0 {
        return format!("{}k", jsrt::number::to_fixed(tokens / 1000.0, 1));
    }
    format!(
        "{}M",
        jsrt::number::to_fixed(tokens / 1_000_000.0, if tokens < 99_950_000.0 { 2 } else { 1 })
    )
}

pub fn format_elapsed(ms: f64) -> String {
    let seconds = jsrt::number::round(ms / 1000.0);
    if seconds < 60.0 {
        return format!("{}s", jsrt::number::to_string(seconds));
    }
    let minutes = (seconds / 60.0).floor();
    if minutes < 60.0 {
        return format!(
            "{}m{:0>2}s",
            jsrt::number::to_string(minutes),
            jsrt::number::to_string(seconds % 60.0)
        );
    }
    format!(
        "{}h{:0>2}m",
        jsrt::number::to_string((minutes / 60.0).floor()),
        jsrt::number::to_string(minutes % 60.0)
    )
}

fn usd(amount: f64) -> String {
    format!(
        "${}",
        jsrt::number::to_fixed(amount, if amount >= 10.0 { 2 } else { 3 })
    )
}

fn n(value: &Value, pointer: &str) -> f64 {
    value.pointer(pointer).and_then(Value::as_f64).unwrap_or(0.0)
}

/// `$1.234`, `$3.287 (sub)`, `unknown`, or a sum of those; `-` when nothing was spent.
fn format_cost(cost: &Value, tokens: f64) -> String {
    let mut parts = Vec::new();
    if let Some(reported) = cost["reportedUsd"].as_f64() {
        parts.push(usd(reported));
    }
    if let Some(sub) = cost["subscriptionUsd"].as_f64() {
        parts.push(if sub > 0.0 {
            format!("{} (sub)", usd(sub))
        } else {
            "subscription".into()
        });
    }
    let unpriced = n(cost, "/unpricedResponses");
    if unpriced > 0.0 {
        parts.push(if parts.is_empty() {
            "unknown".into()
        } else {
            format!("unknown ({} unpriced)", jsrt::number::to_string(unpriced))
        });
    }
    if parts.is_empty() {
        return if tokens > 0.0 { "unknown".into() } else { "-".into() };
    }
    parts.join(" + ")
}

fn format_date(at: f64, options: &TextOptions) -> String {
    let f = if options.utc {
        jsrt::localtime::fields(at, 0)
    } else {
        options.zone.fields(at)
    };
    format!("{}-{:02}-{:02} {:02}:{:02}", f.year, f.month, f.day, f.hour, f.minute)
}

fn short_path(path: &str, home: &str) -> String {
    if !home.is_empty() && (path == home || path.starts_with(&format!("{home}/"))) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    }
}

fn format_bytes(bytes: f64) -> String {
    if bytes < 1024.0 {
        return format!("{} B", jsrt::number::to_string(bytes));
    }
    if bytes < 1024.0 * 1024.0 {
        return format!("{} kB", jsrt::number::to_fixed(bytes / 1024.0, 1));
    }
    format!("{} MB", jsrt::number::to_fixed(bytes / (1024.0 * 1024.0), 1))
}

fn plural(count: f64, one: &str) -> String {
    format!(
        "{} {}{}",
        jsrt::number::to_string(count),
        one,
        if count == 1.0 { "" } else { "s" }
    )
}

fn non_zero(pairs: &[(&str, f64)]) -> String {
    pairs
        .iter()
        .filter(|(_, c)| *c > 0.0)
        .map(|(label, c)| format!("{} {label}", jsrt::number::to_string(*c)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn entries(value: &Value) -> Vec<(&String, &Value)> {
    value.as_object().map_or_default(jsrt::json::object_entries)
}

fn apis_line(apis: &Value) -> String {
    entries(apis)
        .into_iter()
        .filter(|(_, c)| c.as_f64().unwrap_or(0.0) > 0.0)
        .map(|(name, c)| format!("{name} {}", jsrt::number::to_string(c.as_f64().unwrap_or(0.0))))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn cells_line(cells: &Value) -> String {
    let count = n(cells, "/count");
    format!(
        "{}{}",
        plural(count, "cell"),
        if count > 0.0 {
            format!(", {} failed", jsrt::number::to_string(n(cells, "/failed")))
        } else {
            String::new()
        }
    )
}

fn tokens_text(bucket: &Value) -> String {
    let unmeasured = n(bucket, "/unmeasured");
    if n(bucket, "/responses") == 0.0 && unmeasured > 0.0 {
        return NOT_RECORDED.into();
    }
    format!(
        "{}{}",
        format_tokens(n(bucket, "/totalTokens")),
        if unmeasured > 0.0 { " +?" } else { "" }
    )
}

fn table(rows: &[Vec<String>], right: &[usize]) -> Vec<String> {
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if widths.len() <= i {
                widths.push(0);
            }
            widths[i] = widths[i].max(jsrt::utf16::len(cell));
        }
    }
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| {
                    if right.contains(&i) {
                        pad_start(cell, widths[i])
                    } else {
                        pad_end(cell, widths[i])
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end_matches(|c: char| jsrt::string::is_space(c))
                .to_string()
        })
        .collect()
}

fn str_of<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

fn missing_reason(unrecorded: &Map<String, Value>, path: &str) -> String {
    match unrecorded.get(path).and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(why) => format!("{NOT_RECORDED} ({why})"),
        None => NOT_RECORDED.into(),
    }
}

fn push_row(lines: &mut Vec<String>, name: &str, text: &str) {
    lines.push(format!("{}{text}", pad_end(name, 11)));
}

fn push_more(lines: &mut Vec<String>, text: &str) {
    lines.push(format!("{}{text}", " ".repeat(11)));
}

fn render_header(lines: &mut Vec<String>, report: &Value, options: &TextOptions) {
    let session = &report["session"];
    let mode = str_of(report, "/mode");
    let name = str_of(session, "/name");
    lines.push(format!(
        "Session {}{}  [{mode}]",
        str_of(session, "/id"),
        if name.is_empty() {
            String::new()
        } else {
            format!("  {name}")
        }
    ));
    lines.push(format!(
        "{}  ·  {} to {}{}  ·  {}",
        short_path(str_of(session, "/cwd"), options.home),
        format_date(n(session, "/createdAt"), options),
        format_date(n(session, "/modifiedAt"), options),
        if options.utc { " UTC" } else { "" },
        format_bytes(n(session, "/bytes"))
    ));
    lines.push(String::new());
}

fn render_turns_and_root(lines: &mut Vec<String>, report: &Value, unrecorded: &Map<String, Value>, partial: &str) {
    let turns = &report["turns"];
    if turns.is_null() {
        push_row(lines, "Turns", &missing_reason(unrecorded, "turns"));
    } else {
        let ended = non_zero(&[
            ("aborted", n(turns, "/aborted")),
            ("failed", n(turns, "/failed")),
            ("running", n(turns, "/running")),
        ]);
        push_row(
            lines,
            "Turns",
            &format!(
                "{}, {} wall{}{partial}",
                plural(n(turns, "/count"), "turn"),
                format_elapsed(n(turns, "/wallMs")),
                if ended.is_empty() {
                    String::new()
                } else {
                    format!(" ({ended})")
                }
            ),
        );
    }
    let root_models = report
        .pointer("/root/models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let root_text = if !root_models.is_empty() {
        root_models
            .iter()
            .map(|m| format!("{} ({})", str_of(m, "/model"), plural(n(m, "/responses"), "response")))
            .collect::<Vec<_>>()
            .join(", ")
    } else if unrecorded
        .get("root.models")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
    {
        missing_reason(unrecorded, "root.models")
    } else {
        "no model response yet".into()
    };
    push_row(lines, "Root", &root_text);
}

fn render_cells(lines: &mut Vec<String>, report: &Value, unrecorded: &Map<String, Value>, partial: &str) {
    let cells = &report["cells"];
    if cells.is_null() {
        push_row(lines, "Cells", &missing_reason(unrecorded, "cells"));
        return;
    }
    push_row(
        lines,
        "Cells",
        &format!(
            "{}{}{partial}",
            cells_line(&cells["total"]),
            if str_of(cells, "/source") == "counters" {
                "  (runtime count)"
            } else {
                ""
            }
        ),
    );
    let apis = apis_line(&cells["total"]["apis"]);
    if !apis.is_empty() {
        push_more(lines, &apis);
    }
    let (sub, other) = (n(cells, "/subagents/count"), n(cells, "/other/count"));
    if sub > 0.0 || other > 0.0 {
        push_more(
            lines,
            &format!(
                "root {} · sub-agents {}{}",
                jsrt::number::to_string(n(cells, "/root/count")),
                jsrt::number::to_string(sub),
                if other > 0.0 {
                    format!(" · other lanes {}", jsrt::number::to_string(other))
                } else {
                    String::new()
                }
            ),
        );
    }
    let tools = entries(&cells["otherTools"]);
    if !tools.is_empty() {
        let text = tools
            .iter()
            .map(|(name, tool)| {
                let failed = n(tool, "/failed");
                format!(
                    "{name} {}{}",
                    jsrt::number::to_string(n(tool, "/calls")),
                    if failed > 0.0 {
                        format!(" ({} failed)", jsrt::number::to_string(failed))
                    } else {
                        String::new()
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
        push_more(lines, &format!("other tools: {text}"));
    }
}

fn render_frames(lines: &mut Vec<String>, report: &Value, frames: &Value) {
    if n(frames, "/count") == 0.0 {
        push_row(lines, "Frames", "0");
        return;
    }
    let states = non_zero(&[
        ("complete", n(frames, "/complete")),
        ("incomplete", n(frames, "/incomplete")),
        ("failed", n(frames, "/failed")),
        ("cancelled", n(frames, "/cancelled")),
        ("running", n(frames, "/running")),
    ]);
    let nested = n(frames, "/nested");
    push_row(
        lines,
        "Frames",
        &format!(
            "{}: {states}{} · {} tokens",
            jsrt::number::to_string(n(frames, "/count")),
            if nested > 0.0 {
                format!(" · {} nested", jsrt::number::to_string(nested))
            } else {
                String::new()
            },
            tokens_text(&report["usage"]["lanes"]["frames"])
        ),
    );
    let calls = &frames["calls"];
    if !calls.is_null() {
        push_more(
            lines,
            &format!(
                "from {} and {}",
                plural(n(calls, "/map"), "rlm.map call"),
                plural(n(calls, "/infer"), "rlm.infer call")
            ),
        );
    }
    let reasons = entries(&frames["incompleteReasons"]);
    if !reasons.is_empty() {
        let text = reasons
            .iter()
            .map(|(r, c)| format!("{r} {}", jsrt::number::to_string(c.as_f64().unwrap_or(0.0))))
            .collect::<Vec<_>>()
            .join(" · ");
        push_more(lines, &format!("incomplete: {text}"));
    }
    for model in frames["byModel"].as_array().into_iter().flatten() {
        push_more(
            lines,
            &format!(
                "{}  {}  {} tokens",
                str_of(model, "/model"),
                plural(n(model, "/count"), "frame"),
                format_tokens(n(model, "/tokens"))
            ),
        );
    }
}

fn render_subagent_models(lines: &mut Vec<String>, subagents: &Value) {
    for model in subagents["byModel"].as_array().into_iter().flatten() {
        let (count, unmeasured) = (n(model, "/count"), n(model, "/unmeasured"));
        let tokens = if unmeasured == count {
            format!("tokens {NOT_RECORDED}")
        } else {
            format!(
                "{} tokens{}",
                format_tokens(n(model, "/tokens")),
                if unmeasured > 0.0 {
                    format!(" ({} {NOT_RECORDED})", jsrt::number::to_string(unmeasured))
                } else {
                    String::new()
                }
            )
        };
        push_more(
            lines,
            &format!("{}  {}  {tokens}", str_of(model, "/model"), plural(count, "sub-agent")),
        );
    }
}

fn render_subagents(lines: &mut Vec<String>, subagents: &Value) {
    if n(subagents, "/count") == 0.0 {
        push_row(lines, "Sub-agents", "0");
        return;
    }
    let states = non_zero(&[
        ("completed", n(subagents, "/completed")),
        ("failed", n(subagents, "/failed")),
        ("cancelled", n(subagents, "/cancelled")),
        ("interrupted", n(subagents, "/interrupted")),
        ("running", n(subagents, "/running")),
    ]);
    let nested = n(subagents, "/nested");
    push_row(
        lines,
        "Sub-agents",
        &format!(
            "{}: {states} · max depth {}{}",
            jsrt::number::to_string(n(subagents, "/count")),
            jsrt::number::to_string(n(subagents, "/maxDepth")),
            if nested > 0.0 {
                format!(" ({} nested)", jsrt::number::to_string(nested))
            } else {
                String::new()
            }
        ),
    );
    let v = &subagents["verdicts"];
    let unverified = n(v, "/unverified");
    push_more(
        lines,
        &format!(
            "verdicts: {} verified · {} contradicted · {} unverified{}",
            jsrt::number::to_string(n(v, "/verified")),
            jsrt::number::to_string(n(v, "/contradicted")),
            jsrt::number::to_string(unverified),
            if unverified > 0.0 {
                format!(
                    " ({})",
                    non_zero(&[
                        ("without a verdict", n(v, "/none")),
                        ("unchecked", n(v, "/unchecked")),
                        ("invalid", n(v, "/invalid")),
                    ])
                )
            } else {
                String::new()
            }
        ),
    );
    render_subagent_models(lines, subagents);
    for worktree in subagents["worktrees"].as_array().into_iter().flatten() {
        push_more(
            lines,
            &format!(
                "worktree {}: {}, merge {}",
                str_of(worktree, "/branch"),
                plural(n(worktree, "/changedFiles"), "file"),
                worktree["merge"].as_str().unwrap_or(NOT_RECORDED)
            ),
        );
    }
}

fn render_other_work(lines: &mut Vec<String>, depth: &Value, unrecorded: &Map<String, Value>) {
    let workflows = &depth["workflows"];
    push_row(
        lines,
        "Workflows",
        &if workflows.is_null() {
            missing_reason(unrecorded, "depth.workflows")
        } else {
            plural(n(workflows, "/runs"), "run")
        },
    );
    let typed = entries(&depth["typedAgents"]["byDefinition"]);
    push_row(
        lines,
        "Other work",
        &format!(
            "{}{} · {}",
            plural(n(depth, "/typedAgents/count"), "typed-agent task"),
            if typed.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    typed
                        .iter()
                        .map(|(d, c)| format!("{d} {}", jsrt::number::to_string(c.as_f64().unwrap_or(0.0))))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
            plural(n(depth, "/backgroundJobs/count"), "background job")
        ),
    );
}

fn usage_row(name: &str, bucket: &Value) -> Vec<String> {
    vec![
        format!("  {name}"),
        jsrt::number::to_string(n(bucket, "/responses")),
        format_tokens(n(bucket, "/input")),
        format_tokens(n(bucket, "/output")),
        format_tokens(n(bucket, "/cacheRead")),
        format_tokens(n(bucket, "/cacheWrite")),
        tokens_text(bucket),
        format_cost(&bucket["cost"], n(bucket, "/totalTokens")),
    ]
}

fn usage_header(first: &str) -> Vec<String> {
    [
        first,
        "responses",
        "input",
        "output",
        "cache read",
        "cache write",
        "total",
        "cost",
    ]
    .map(String::from)
    .to_vec()
}

fn render_usage_notes(lines: &mut Vec<String>, total: &Value, mode: &str) {
    let mut notes = Vec::new();
    if mode == "ultron claude" {
        notes.push(
            "root n/r: Claude Code owns the root conversation; its tokens and cost are Claude Code's, not in this session."
                .to_string(),
        );
    }
    if !total["cost"]["subscriptionUsd"].is_null() {
        notes.push(
            "(sub): made on a subscription login; the amount is the provider's notional figure, not a charge.".into(),
        );
    }
    let unpriced = n(total, "/cost/unpricedResponses");
    if unpriced > 0.0 {
        notes.push(format!(
            "unknown: {} used tokens and reported no price.",
            plural(unpriced, "response")
        ));
    }
    let unmeasured = n(total, "/unmeasured");
    if unmeasured > 0.0 {
        notes.push(format!(
            "+?: {} reported no usage; their tokens are not in the totals.",
            plural(unmeasured, "sub-agent")
        ));
    }
    let cache_read = n(total, "/cacheRead");
    let prompt_tokens = n(total, "/input") + cache_read;
    if cache_read > 0.0 && prompt_tokens > 0.0 {
        let pct = jsrt::number::round(cache_read / prompt_tokens * 100.0);
        notes.push(format!(
            "cache hit: {}% ({} cached of {} prompt tokens).",
            jsrt::number::to_string(pct),
            format_tokens(cache_read),
            format_tokens(prompt_tokens),
        ));
    }
    for note in notes {
        lines.push(format!("  {note}"));
    }
}

fn render_tokens_and_cost(lines: &mut Vec<String>, report: &Value, mode: &str) {
    lines.push(String::new());
    lines.push("Tokens and cost".into());
    let usage = &report["usage"];
    let lanes = &usage["lanes"];
    let mut rows = vec![usage_header("  lane")];
    if mode == "ultron claude" && n(lanes, "/root/responses") == 0.0 {
        let mut r = vec!["  root".to_string()];
        r.extend(std::iter::repeat_n("n/r".to_string(), 7));
        rows.push(r);
    } else {
        rows.push(usage_row("root", &lanes["root"]));
    }
    rows.push(usage_row("frames", &lanes["frames"]));
    rows.push(usage_row("sub-agents", &lanes["subagents"]));
    if n(lanes, "/other/responses") > 0.0 || n(lanes, "/other/unmeasured") > 0.0 {
        rows.push(usage_row("other", &lanes["other"]));
    }
    rows.push(usage_row("total", &usage["total"]));
    let numeric = [1, 2, 3, 4, 5, 6];
    lines.extend(table(&rows, &numeric));
    let models = usage["models"].as_array().cloned().unwrap_or_default();
    if !models.is_empty() {
        let mut model_rows = vec![usage_header("  model")];
        for model in &models {
            model_rows.push(usage_row(str_of(model, "/model"), model));
        }
        lines.extend(table(&model_rows, &numeric));
    }
    render_usage_notes(lines, &usage["total"], mode);
}

fn render_guardrails(
    lines: &mut Vec<String>,
    report: &Value,
    unrecorded: &Map<String, Value>,
    counters_since: Option<f64>,
    options: &TextOptions,
) {
    lines.push(String::new());
    lines.push("Guardrails".into());
    let guardrails = &report["guardrails"];
    let since = counters_since.map_or(String::new(), |s| {
        format!(" (counted since {})", format_date(s, options))
    });
    let guards = &guardrails["guards"];
    if guards.is_null() {
        push_row(lines, "  Loki", &missing_reason(unrecorded, "guardrails.guards"));
    } else if entries(guards).is_empty() {
        push_row(lines, "  Loki", &format!("no write was checked{since}"));
    } else {
        for (name, g) in entries(guards) {
            push_row(
                lines,
                &format!("  {name}"),
                &format!(
                    "{}, {} blocked, {} unchecked · after cells: {}, {} · {}{since}",
                    plural(n(g, "/checks"), "check"),
                    jsrt::number::to_string(n(g, "/blocked")),
                    jsrt::number::to_string(n(g, "/unchecked")),
                    plural(n(g, "/afterChecks"), "check"),
                    plural(n(g, "/afterFindings"), "finding"),
                    format_elapsed(n(g, "/ms"))
                ),
            );
        }
    }
    let secrets = &guardrails["secretsMasked"];
    push_row(
        lines,
        "  Secrets",
        &if secrets.is_null() {
            missing_reason(unrecorded, "guardrails.secretsMasked")
        } else {
            format!("{} masked", jsrt::number::to_string(secrets.as_f64().unwrap_or(0.0)))
        },
    );
    let hints = entries(&guardrails["hints"]);
    let stuck = guardrails["hints"]["stuck-loop"].as_f64().unwrap_or(0.0);
    push_row(
        lines,
        "  Hints",
        &format!(
            "stuck-loop {}{}",
            jsrt::number::to_string(stuck),
            hints
                .iter()
                .filter(|(tag, _)| tag.as_str() != "stuck-loop")
                .map(|(tag, c)| format!(" · {tag} {}", jsrt::number::to_string(c.as_f64().unwrap_or(0.0))))
                .collect::<String>()
        ),
    );
    let nudges = &guardrails["nudges"];
    push_row(
        lines,
        "  Nudges",
        &if nudges.is_null() {
            missing_reason(unrecorded, "guardrails.nudges")
        } else {
            format!(
                "tool rounds {} · wait {} · skill {}",
                jsrt::number::to_string(n(nudges, "/toolRounds")),
                jsrt::number::to_string(n(nudges, "/wait")),
                jsrt::number::to_string(n(nudges, "/skill"))
            )
        },
    );
    let limits = &guardrails["usageLimitBlocks"];
    push_row(
        lines,
        "  Limits",
        &if limits.is_null() {
            missing_reason(unrecorded, "guardrails.usageLimitBlocks")
        } else {
            format!("{}{since}", plural(limits.as_f64().unwrap_or(0.0), "usage-limit block"))
        },
    );
}

fn render_memory(lines: &mut Vec<String>, report: &Value, unrecorded: &Map<String, Value>) {
    lines.push(String::new());
    lines.push("Memory".into());
    let operations = &report["memory"]["operations"];
    if operations.is_null() {
        push_row(lines, "  Store", &missing_reason(unrecorded, "memory.operations"));
    } else {
        let text = entries(operations)
            .iter()
            .map(|(op, c)| format!("{op} {}", jsrt::number::to_string(c.as_f64().unwrap_or(0.0))))
            .collect::<Vec<_>>()
            .join(" · ");
        push_row(lines, "  Store", &text);
    }
    let recall = &report["memory"]["recall"];
    if recall.is_object() {
        let count = |key: &str| recall[key].as_f64().unwrap_or(0.0);
        let memories = count("memories");
        let mut text = format!(
            "{} · {} {}",
            plural(count("messages"), "automatic recall"),
            jsrt::number::to_string(memories),
            if memories == 1.0 { "memory" } else { "memories" }
        );
        if count("briefs") > 0.0 {
            text.push_str(&format!(" · {}", plural(count("briefs"), "project brief")));
        }
        if count("stale") > 0.0 {
            text.push_str(&format!(" · {} stale", jsrt::number::to_string(count("stale"))));
        }
        push_row(lines, "  Recall", &text);
    }
}

/// The full report of one session, as lines.
pub fn render_session_report(report: &Value, options: &TextOptions) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let empty = Map::new();
    let unrecorded = report["unrecorded"].as_object().unwrap_or(&empty);
    let depth = &report["depth"];
    let mode = str_of(report, "/mode");
    render_header(&mut lines, report, options);
    push_row(&mut lines, "Depth", str_of(depth, "/verdict"));
    let counters_since = report.pointer("/guardrails/countersSince").and_then(Value::as_f64);
    let partial = match counters_since {
        Some(since) if mode == "ultron claude" => format!(" (counted since {})", format_date(since, options)),
        _ => String::new(),
    };
    render_turns_and_root(&mut lines, report, unrecorded, &partial);
    render_cells(&mut lines, report, unrecorded, &partial);
    render_frames(&mut lines, report, &depth["frames"]);
    render_subagents(&mut lines, &depth["subagents"]);
    render_other_work(&mut lines, depth, unrecorded);
    render_tokens_and_cost(&mut lines, report, mode);
    render_guardrails(&mut lines, report, unrecorded, counters_since, options);
    render_memory(&mut lines, report, unrecorded);
    lines
}

fn table_model(report: &Value) -> String {
    let models = report.pointer("/root/models").and_then(Value::as_array);
    match models.and_then(|m| m.first()) {
        Some(first) => {
            let model = str_of(first, "/model").to_string();
            let count = models.map_or(0, Vec::len);
            if count > 1 {
                format!("{model} +{}", count - 1)
            } else {
                model
            }
        }
        None if str_of(report, "/mode") == "ultron claude" => "Claude Code".into(),
        None => "-".into(),
    }
}

fn clip(text: &str, width: usize) -> String {
    if jsrt::utf16::len(text) <= width {
        text.to_string()
    } else {
        format!("…{}", jsrt::utf16::skip(text, jsrt::utf16::len(text) - width + 1))
    }
}

/// One line per session, in the order given.
pub fn render_session_table(reports: &[Value], options: &TextOptions) -> Vec<String> {
    let mut rows: Vec<Vec<String>> = vec![
        [
            "LAST ACTIVE",
            "ID",
            "CWD",
            "MODE",
            "MODEL",
            "TURNS",
            "CELLS",
            "FRAMES",
            "SUBS",
            "TOKENS",
            "COST",
            "DEPTH",
        ]
        .map(String::from)
        .to_vec(),
    ];
    let mut unrecorded = false;
    for report in reports {
        let mut count = |value: Option<f64>| match value {
            None => {
                unrecorded = true;
                "n/r".to_string()
            }
            Some(v) => jsrt::number::to_string(v),
        };
        let total = &report["usage"]["total"];
        let unknown_usage = str_of(report, "/mode") == "ultron claude"
            && n(total, "/responses") == 0.0
            && n(total, "/unmeasured") == 0.0;
        let id = str_of(report, "/session/id");
        let turns = report.pointer("/turns/count").and_then(Value::as_f64);
        let cells = report.pointer("/cells/total/count").and_then(Value::as_f64);
        let turns = count(turns);
        let cells = count(cells);
        let (tokens, cost) = if unknown_usage {
            (count(None), count(None))
        } else {
            (
                tokens_text(total),
                format_cost(&total["cost"], n(total, "/totalTokens")),
            )
        };
        rows.push(vec![
            format_date(n(report, "/session/modifiedAt"), options),
            jsrt::utf16::head(id, 13).to_string(),
            clip(&short_path(str_of(report, "/session/cwd"), options.home), 32),
            str_of(report, "/mode").to_string(),
            clip(&table_model(report), 34),
            turns,
            cells,
            jsrt::number::to_string(n(report, "/depth/frames/count")),
            jsrt::number::to_string(n(report, "/depth/subagents/count")),
            tokens,
            cost,
            str_of(report, "/depth/verdict").to_string(),
        ]);
    }
    let mut lines = table(&rows, &[5, 6, 7, 8, 9]);
    if unrecorded {
        lines.push(
            "n/r: not recorded by that session (never zero). Tokens and cost of an `ultron claude` session leave out the root, which is Claude Code's."
                .into(),
        );
    }
    lines
}

/// `text.padEnd(width)`, in UTF-16 code units.
fn pad_end(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(jsrt::utf16::len(text))))
}

/// `text.padStart(width)`, in UTF-16 code units.
fn pad_start(text: &str, width: usize) -> String {
    format!("{}{text}", " ".repeat(width.saturating_sub(jsrt::utf16::len(text))))
}
