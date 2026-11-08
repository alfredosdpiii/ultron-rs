//! Session usage (`ultron/usage-cli.ts`): a session file read into its usage log ([`log`]), the session report built
//! from it ([`report`], with the cell statistics of [`stats`]) and the report as text ([`text`]). `ultron usage`,
//! the TUI's `/usage` and `ultron mcp`'s `usage.report` inspection all show this report.

pub mod log;
pub mod report;
pub mod stats;
pub mod text;

use std::path::Path;

/// The session report of the session file `path` as `/usage` builds it: `providers` (those on a subscription login,
/// `subscriptionProviders`) are priced as notional.
pub fn session_report(path: &Path, providers: &[String]) -> Result<serde_json::Value, String> {
    let log = log::read_session_log(path).map_err(|e| e.message().to_string())?;
    Ok(report::build_session_report(&log, providers))
}
