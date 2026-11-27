use super::{ABORT_REQUESTED, rlm_tool, text_block};
use crate::compose::{Config, Runtime, RuntimeEvents, Stack};
use agent::{AgentTool, AgentToolResult, ToolUpdate};
use ai::http::BoxFuture;
use ai::{AbortSignal, Message, Model, Tool, UserContent};
use host::Host;
use host::cell::{CellFormat, cell_output};
use host::secrets::OutputMasker;
use kernel::Kernel;
use lanes::{CellOutcome, Lanes};
use serde_json::{Value, json};
use session_journal::run::MAIN_LANE;
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

/// Resolves with `work`'s output, or `None` once `signal` aborts first.
async fn until_aborted<T>(work: impl Future<Output = T>, signal: &AbortSignal) -> Option<T> {
    let mut work = pin!(work);
    let mut cancelled = pin!(signal.cancelled());
    std::future::poll_fn(|cx| {
        if let Poll::Ready(value) = work.as_mut().poll(cx) {
            return Poll::Ready(Some(value));
        }
        match cancelled.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// How long a cell may run before its tool call returns with the cell still running (detached). Cells that park
/// on a host-side wait detach at once; in a lane nothing wakes, others run as long as they need, as in TypeScript.
const CELL_BUDGET: Duration = Duration::from_hours(24);

/// How long a woken lane's cell holds its tool call by default before it continues in the background: short, so
/// the user can always keep chatting (anything a cell waits on, `bash` included, yields with it).
const DEFAULT_CELL_YIELD_AFTER: Duration = Duration::from_secs(10);

/// `ULTRON_CELL_YIELD_AFTER` seconds (default 10): a woken lane's cell still running then is detached, so the turn
/// ends and its end arrives as a `cell_done` event. `0`, `off`, a negative or a non-finite number keep the cell in its
/// tool call until it ends; an unreadable value is the default.
pub fn cell_yield_after(raw: Option<&str>) -> Option<Duration> {
    let raw = raw.map_or_default(|raw| raw.trim().to_lowercase());
    if raw.is_empty() {
        return Some(DEFAULT_CELL_YIELD_AFTER);
    }
    if jsrt::env::flag_off_or_none(Some(&raw)) {
        return None;
    }
    match raw.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() && seconds > 0.0 => Some(Duration::from_secs_f64(seconds)),
        Ok(_) => None,
        Err(_) => Some(DEFAULT_CELL_YIELD_AFTER),
    }
}

/// The `rlm` tool (`createUltronRlmTool` for one lane): each call runs its cell in the lane of the session's REPL
/// lanes (crates/delegation `Lanes`). Aborting the turn cancels the running cell.
pub struct Rlm {
    tool: Tool,
    lanes: Lanes,
    lane: String,
    /// The session's masker: values MCP servers resolve (env, headers) are registered in it.
    masker: Arc<OutputMasker>,
    mask_secrets: Option<String>,
    max_bytes: usize,
    spill_dir: PathBuf,
    /// Situational hints (`ULTRON_HINTS=on`; the loop guards without it): one `[hint:<tag>]` line at the end of a
    /// cell's result.
    hints: Option<Arc<kernel_apis::cell_hints::CellHints>>,
    /// The images a cell attaches with `view_image` (crate::view_images).
    images: Arc<crate::view_images::ViewImages>,
    /// The lane's host, whose after-cell file hooks see each cell start and end; what they found arrives with
    /// the lane's next cell result.
    host: Option<Arc<Host>>,
    /// The budget line a cell's result ends with (crate::budget_note), once wired.
    budget: Arc<std::sync::OnceLock<crate::budget_note::BudgetNote>>,
    /// The session's runtime counters: each cell is counted where it runs.
    stats: Option<Arc<crate::session_stats::SessionStats>>,
    /// The context-pressure reminder a cell's result ends with when the lane nears its limit
    /// (crate::context_reminder).
    reminders: Option<Arc<crate::context_reminder::ContextReminders>>,
    /// Whether the host wakes this lane when its work ends (crate::wake): how a detached cell's note words its end,
    /// and whether a slow cell yields its tool call ([`cell_yield_after`]).
    wake: crate::wake::RootWake,
    /// How long a woken lane's cell holds its tool call ([`cell_yield_after`]); `None`: until it ends.
    yield_after: Option<Duration>,
}

impl Rlm {
    /// How long this lane's next cell holds its tool call: a woken lane's slow cell yields after
    /// [`cell_yield_after`] (its end wakes the lane); any other lane's waits, as nothing would deliver its end.
    fn cell_budget(&self) -> Duration {
        match self.yield_after {
            Some(after) if self.wake.woken(&self.lane) => after,
            _ => CELL_BUDGET,
        }
    }

    /// This tool with the stack's wake rule (crate::wake) for its detached-cell note.
    pub(crate) fn with_wake(mut self, wake: crate::wake::RootWake) -> Rlm {
        self.wake = wake;
        self
    }

    pub(crate) fn with_budget_note(mut self, budget: Arc<std::sync::OnceLock<crate::budget_note::BudgetNote>>) -> Rlm {
        self.budget = budget;
        self
    }

    /// This tool with its cells counted in `stats` (`stats.cell(lane, code, {failed, masked})`).
    pub(crate) fn with_stats(mut self, stats: Arc<crate::session_stats::SessionStats>) -> Rlm {
        self.stats = Some(stats);
        self
    }

    pub(crate) fn with_context_reminders(mut self, reminders: Arc<crate::context_reminder::ContextReminders>) -> Rlm {
        self.reminders = Some(reminders);
        self
    }

    /// The context reminder this lane's result `text` (with `images` images) ends with, if it is due.
    fn context_reminder(&self, text: &str, images: usize) -> Option<String> {
        self.reminders.as_ref()?.reminder(&self.lane, text, images)
    }

    /// An error result's text, ending with the context reminder when it is due.
    fn with_reminder(&self, text: String) -> String {
        match self.context_reminder(&text, 0) {
            Some(line) => format!("{text}\n{line}"),
            None => text,
        }
    }

    fn count_cell(&self, code: &str, failed: bool, masked: usize) {
        if let Some(stats) = &self.stats {
            stats.cell(&self.lane, code, failed, masked);
        }
    }

    /// The budget line this cell's result ends with, if any (`budgetNote(lane, operationId)`).
    fn budget_line(&self) -> Option<String> {
        self.budget.get().and_then(|note| note(&self.lane))
    }

    /// The hint the cell earned with this result text (or error message), if any.
    fn hint(&self, text: &str, ename: Option<&str>) -> Option<String> {
        let outcome = kernel_apis::cell_hints::CellOutcome { text, ename };
        self.hints
            .as_ref()
            .and_then(|hints| hints.end_cell(&self.lane, &outcome))
    }

    /// The cell's result text (or error message) with the hint its cell earned, if any.
    fn with_hint(&self, text: String, ename: Option<&str>) -> String {
        match self.hint(&text, ename) {
            Some(hint) => format!("{text}\n{hint}"),
            None => text,
        }
    }

    /// Tells the file hooks this lane's cell ended; what they found since earlier cells, to go with its result.
    fn end_cell(&self) -> Option<String> {
        let host = self.host.as_ref()?;
        host.cell_ended(&self.lane);
        host.take_pending(&self.lane)
    }

    /// How this lane's cell results are finished (masking, the byte budget, the spill directory).
    fn cell_format(&self) -> CellFormat<'_> {
        CellFormat {
            masker: &self.masker,
            mask_secrets: self.mask_secrets.as_deref(),
            max_bytes: self.max_bytes,
            spill_dir: Some(&self.spill_dir),
        }
    }
}

impl AgentTool for Rlm {
    fn tool(&self) -> &Tool {
        &self.tool
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        args: Value,
        signal: &'a AbortSignal,
        _update: &'a ToolUpdate<'a>,
    ) -> BoxFuture<'a, Result<AgentToolResult, String>> {
        Box::pin(self.run_cell(args, signal))
    }
}

impl Rlm {
    /// One cell, as its result text (or error message). A cell run in a side lane because this lane is still busy
    /// (`Lanes::start_routed`) starts its result with the side-lane note.
    async fn run_cell(&self, args: Value, signal: &AbortSignal) -> Result<AgentToolResult, String> {
        let note = Mutex::new(None);
        let result = self.run_routed_cell(args, signal, &note).await;
        let Some(note) = jsrt::sync::lock(&note).take() else {
            return result;
        };
        match result {
            Err(text) => Err(format!("{note}\n{text}")),
            Ok(mut result) => {
                match result.content.iter_mut().find_map(|part| match part {
                    UserContent::Text(text) => Some(text),
                    UserContent::Image(_) => None,
                }) {
                    Some(text) => text.text = format!("{note}\n{}", text.text),
                    None => result.content.insert(0, text_block(note)),
                }
                Ok(result)
            }
        }
    }

    async fn run_routed_cell(
        &self,
        args: Value,
        signal: &AbortSignal,
        note: &Mutex<Option<String>>,
    ) -> Result<AgentToolResult, String> {
        let code = args["code"].as_str().ok_or("rlm needs a `code` string")?.to_string();
        self.begin_routed_cell(&code)?;
        let outcome = self.execute_routed_cell(&code, signal, note).await?;
        let has_result = self.count_non_result_outcome(&code, &outcome);
        let findings = self.end_cell();
        let images = self.images.take(&self.lane);
        let shown = self.images.take_display(&self.lane);
        let outcome = self.unwrap_cell_outcome(outcome, findings.as_deref())?;
        if signal.aborted() {
            if has_result {
                self.count_cell(&code, false, 0);
            }
            return Err(with_optional_line(ABORT_REQUESTED.into(), findings.as_deref()));
        }
        match outcome {
            CellOutcome::Running { cell, parked } => self.finish_running_cell(cell, parked, findings.as_deref()),
            CellOutcome::Done(result) => {
                let result = result.map_err(|e| {
                    self.with_reminder(with_optional_line(
                        self.with_hint(e, Some("Error")),
                        findings.as_deref(),
                    ))
                })?;
                self.finish_done_cell(&code, &result, findings.as_deref(), images, &shown)
            }
        }
    }

    fn count_non_result_outcome(&self, code: &str, outcome: &Result<Result<CellOutcome, String>, String>) -> bool {
        let has_result = matches!(outcome, Ok(Ok(CellOutcome::Done(Ok(_)))));
        if !has_result {
            let left_running = matches!(outcome, Ok(Ok(CellOutcome::Running { .. })));
            self.count_cell(code, !left_running, 0);
        }
        has_result
    }

    fn unwrap_cell_outcome(
        &self,
        outcome: Result<Result<CellOutcome, String>, String>,
        findings: Option<&str>,
    ) -> Result<CellOutcome, String> {
        outcome
            .map_err(|e| self.with_reminder(with_optional_line(e, findings)))?
            .map_err(|e| self.with_reminder(with_optional_line(e, findings)))
    }

    fn begin_routed_cell(&self, code: &str) -> Result<(), String> {
        if let Some(hints) = &self.hints {
            if let Some(refusal) = hints.refusal(&self.lane, code) {
                return Err(refusal);
            }
            hints.begin_cell(&self.lane, code);
        }
        self.images.begin(&self.lane);
        if let Some(host) = &self.host {
            host.cell_started(&self.lane);
        }
        Ok(())
    }

    async fn execute_routed_cell(
        &self,
        code: &str,
        signal: &AbortSignal,
        note: &Mutex<Option<String>>,
    ) -> Result<Result<Result<CellOutcome, String>, String>, String> {
        let (lanes, lane, source) = (self.lanes.clone(), self.lane.clone(), code.to_string());
        let started = tokio::task::spawn_blocking(move || lanes.start_routed(&lane, &source))
            .await
            .map_err(|e| e.to_string())
            .and_then(|started| started);
        let id = match started {
            Ok(started) => {
                *jsrt::sync::lock(note) = started.note.clone();
                started.cell
            }
            Err(error) => return Ok(Ok(Err(error))),
        };
        let (lanes, wait_id, budget) = (self.lanes.clone(), id.clone(), self.cell_budget());
        let mut cell = tokio::task::spawn_blocking(move || lanes.wait(&wait_id, budget));
        match until_aborted(&mut cell, signal).await {
            Some(done) => Ok(done.map_err(|e| e.to_string())),
            None => {
                let _ = self.lanes.cancel(&id, ABORT_REQUESTED);
                let _ = cell.await;
                self.count_cell(code, true, 0);
                let findings = self.end_cell();
                self.images.take(&self.lane);
                self.images.take_display(&self.lane);
                Err(with_optional_line(ABORT_REQUESTED.into(), findings.as_deref()))
            }
        }
    }

    fn finish_running_cell(
        &self,
        cell: String,
        parked: bool,
        findings: Option<&str>,
    ) -> Result<AgentToolResult, String> {
        if !parked {
            self.lanes.detach(&cell)?;
        }
        let body = if parked {
            detached_note(&cell, self.wake.woken(&self.lane))
        } else {
            yielded_note(&cell, self.cell_budget())
        };
        Ok(AgentToolResult {
            content: vec![text_block(with_optional_line(body, findings))],
            details: json!({}),
            usage: None,
            terminate: None,
        })
    }

    fn finish_done_cell(
        &self,
        code: &str,
        result: &kernel::ExecutionResult,
        findings: Option<&str>,
        images: Option<host::view_image::CellImages>,
        shown: &[crate::view_images::DisplayImage],
    ) -> Result<AgentToolResult, String> {
        let output = cell_output(result, &self.cell_format());
        self.count_cell(
            code,
            output.is_err(),
            output
                .as_ref()
                .map_or_else(|failure| failure.masked, |output| output.masked),
        );
        let output = output.map_err(|failure| {
            self.with_reminder(with_optional_line(
                self.with_hint(failure.message, Some(&failure.ename)),
                findings,
            ))
        })?;
        let hint = self.hint(&output.text, None);
        let base = if output.text.is_empty() {
            "(no result)".to_string()
        } else {
            output.text
        };
        let text = with_optional_line(crate::view_images::with_display_lines(base, shown), findings);
        let text = with_optional_line(text, hint.as_deref());
        let text = with_optional_line(text, self.budget_line().as_deref());
        let image_count = images.as_ref().map_or(0, |i| i.images.len());
        let reminder = self.context_reminder(&text, image_count);
        let text = with_optional_line(text, reminder.as_deref());
        Ok(AgentToolResult {
            content: crate::view_images::cell_content(images.as_ref(), text),
            details: crate::view_images::with_display_details(
                self.host.as_ref().map_or_else(
                    || json!({}),
                    |host| {
                        let mask = |text: &str| self.masker.mask_counted(text, self.mask_secrets.as_deref()).0;
                        crate::cell_file_changes::details(host, &self.lane, &mask)
                    },
                ),
                shown,
            ),
            usage: None,
            terminate: None,
        })
    }
}

fn with_optional_line(text: String, line: Option<&str>) -> String {
    match line {
        Some(line) => format!("{text}\n{line}"),
        None => text,
    }
}

/// One session's root lane: the `rlm` tool over the "main" REPL lane of the session's host stack.
pub struct Lane {
    /// The host operations (shell, writes) behind the stack, for host-side commands such as RPC `bash`.
    pub host: Arc<Host>,
    pub stack: Arc<Stack>,
    pub tool: Arc<Rlm>,
    /// Completions of work the session's runs started (subagents, detached cells), delivered between turns.
    pub events: Arc<RuntimeEvents>,
}

impl Lane {
    /// Start the root lane ("main") of `session_id` over `runtime`'s host stack.
    pub fn open(runtime: &Runtime, model: &Model, api_key: Option<String>, session_id: &str) -> Result<Lane, String> {
        Lane::open_with(runtime, model, api_key, session_id, None)
    }

    /// [`Lane::open`] whose subagents run as `subagents` says.
    pub fn open_with(
        runtime: &Runtime,
        model: &Model,
        api_key: Option<String>,
        session_id: &str,
        subagents: Option<crate::compose::SubagentHost>,
    ) -> Result<Lane, String> {
        let stack = runtime.stack_with(model, api_key, session_id, subagents)?;
        let tool = Arc::new(stack.rlm(MAIN_LANE)?);
        Ok(Lane {
            host: stack.host.clone(),
            events: stack.events.clone(),
            tool,
            stack,
        })
    }

    /// The transcript's first message: the system prompt and the declared `rlm` tool.
    pub fn system_message(&self, system: &str) -> Option<Message> {
        ai::transcript::create_initial_system_message(Some(system), Some(std::slice::from_ref(&self.tool.tool)))
            .map(Message::System)
    }

    /// The model's tool list: exactly the lane's `rlm` tool.
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        vec![self.tool.clone() as Arc<dyn AgentTool>]
    }

    /// The root lane's kernel, once open.
    pub fn kernel(&self) -> Option<Arc<Kernel>> {
        self.stack.lanes.kernel(MAIN_LANE)
    }

    pub fn shutdown(&self) {
        self.stack.close();
    }

    /// Shut down for a successor worker: shell jobs keep running for it.
    pub fn hand_over(&self) {
        self.stack.hand_over();
    }
}

/// The result of a call whose cell keeps running detached: how its end reaches a lane the host wakes (`woken`: the
/// end starts a new turn) or one nothing wakes (the end is reported with a later result; stop it or check on it).
pub fn detached_note(cell: &str, woken: bool) -> String {
    if woken {
        format!(
            "Cell {cell} is still running in the background (it waits on the host); its end arrives as a <runtime_event kind=\"cell_done\" id=\"{cell}\">. Do other work or end your turn."
        )
    } else {
        format!(
            "Cell {cell} is still running in the background (it waits on the host); its end is reported as a <runtime_event kind=\"cell_done\" id=\"{cell}\"> with a later rlm result, and nothing wakes you after you reply. Do other work meanwhile; `await rlm.cells()` lists it while it runs and `await rlm.cancel(\"{cell}\")` stops it."
        )
    }
}

/// The tool result of a cell that ran past its budget and continues in the background (only a woken lane's: see
/// [`Rlm::cell_budget`]).
pub fn yielded_note(cell: &str, after: Duration) -> String {
    format!(
        "Cell {cell} was still running after {} s, so it continues in the background; its end (with its result) arrives as a <runtime_event kind=\"cell_done\" id=\"{cell}\">. Do other work or end your turn. Its variables are not in your next cell until it ends (that cell runs in a side lane); `await rlm.cancel(\"{cell}\")` stops it.",
        after.as_secs_f64()
    )
}

pub(crate) fn rlm_over(
    config: &Config,
    lanes: Lanes,
    lane: &str,
    spill_dir: PathBuf,
    hints: Option<Arc<kernel_apis::cell_hints::CellHints>>,
    images: Arc<crate::view_images::ViewImages>,
    host: Option<Arc<Host>>,
) -> Rlm {
    let mut tool = rlm_tool();
    let listing = kernel_apis::CodeSkills::new(kernel_apis::CodeSkillsOptions::new(
        &config.skills_dir,
        &config.runtime_path,
    ))
    .describe();
    if !listing.is_empty() {
        tool.description = format!("{}\n{listing}", tool.description);
    }
    Rlm {
        tool,
        lanes,
        lane: lane.to_string(),
        masker: config.masker.clone(),
        mask_secrets: config.mask_secrets.clone(),
        max_bytes: kernel::rlm_output_budget(config.output_bytes.as_deref()) as usize,
        spill_dir,
        hints,
        images,
        host,
        budget: Arc::new(std::sync::OnceLock::new()),
        stats: None,
        reminders: None,
        wake: crate::wake::RootWake::default(),
        yield_after: cell_yield_after(config.cell_yield_after.as_deref()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_yield_after_defaults_to_ten_seconds_and_can_be_turned_off() {
        assert_eq!(cell_yield_after(None), Some(Duration::from_secs(10)));
        assert_eq!(cell_yield_after(Some("  ")), Some(Duration::from_secs(10)));
        assert_eq!(cell_yield_after(Some("2.5")), Some(Duration::from_millis(2500)));
        assert_eq!(cell_yield_after(Some("junk")), Some(Duration::from_secs(10)));
        for off in ["0", "off", "OFF", "none", "-1", "inf", "NaN"] {
            assert_eq!(cell_yield_after(Some(off)), None, "{off}");
        }
    }

    #[test]
    fn a_yielded_cell_says_its_end_arrives_as_an_event() {
        let note = yielded_note("cell-7", Duration::from_secs(10));
        assert!(note.starts_with("Cell cell-7 was still running after 10 s"));
        assert!(note.contains("<runtime_event kind=\"cell_done\" id=\"cell-7\">"));
        assert!(note.contains("end your turn"));
    }
}
