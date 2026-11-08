//! `HindsightLessons` of `autoreview/lessons.ts`: autoreview's fates and rules retained in Hindsight, in the
//! "ultron-autoreview" bank on the same server the REPL's memory uses, tagged per repository, as tool evidence. One
//! retain per fate and per rule, each with a document id that replaces the previous version.

use crate::ledger::{FATE_DAYS, Fate, FateKind, Lesson};
use hindsight::{HindsightBackend, HindsightOptions, Transport};
use kernel::Signal;
use kernel_apis::MemoryError;
use kernel_apis::memory::MemoryBackend;
use serde_json::json;
use std::sync::Arc;

const BANK: &str = "ultron-autoreview";
const GUIDE_QUERY: &str = "Which findings does this repository's team accept and which do they reject? Name the \
    passes, categories, files and patterns that produce findings they reject, the kinds of finding they reliably \
    fix, and the review rules that follow (report, report as a low note, or leave out), each with the evidence \
    count behind it. Only patterns seen in more than one finding.";

fn repo_identity(repo: &str) -> String {
    jsrt::hash::sha256_hex(repo.to_lowercase().as_bytes())[..16].to_string()
}

pub struct HindsightLessons {
    backend: HindsightBackend,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl HindsightLessons {
    /// `new HindsightLessons({baseUrl, now})`: INVALID_CONFIG for a URL the client refuses.
    pub fn new(
        base_url: &str,
        now: Box<dyn Fn() -> i64 + Send + Sync>,
        transport: Option<Arc<dyn Transport>>,
    ) -> Result<Self, MemoryError> {
        let tags = json!({"project": ["ultron:autoreview:project:bank"]});
        let mut options = HindsightOptions::new(base_url, BANK, tags.as_object().cloned().unwrap_or_default());
        options.ensure_bank = true;
        options.transport = transport;
        Ok(Self {
            backend: HindsightBackend::new(options)?,
            now,
        })
    }

    /// The learned review guide of `repo`: the mental model `autoreview-<identity>`, which Hindsight rewrites from
    /// the repository's fates and rules after each consolidation, at most hourly. The first call for a repository
    /// creates it (its first content arrives in the background); `None` while it has no content.
    pub fn guide(&self, repo: &str) -> Result<Option<String>, String> {
        let identity = repo_identity(repo);
        let id = format!("autoreview-{identity}");
        let signal = Signal::new();
        let model = self.backend.mental_model(&id, &signal)?;
        if model.is_null() {
            let request = json!({
                "id": id, "name": format!("Autoreview lessons for {repo}"),
                "source_query": format!("{GUIDE_QUERY} The repository is {repo}."),
                "tags": [format!("ultron:autoreview:project:{identity}")], "max_tokens": 1024,
                "trigger": {
                    "refresh_after_consolidation": true, "min_refresh_interval_seconds": 3600, "mode": "delta",
                    "budget": "low", "tags_match": "any_strict",
                },
            });
            self.backend.create_mental_model(&request, &signal)?;
            return Ok(None);
        }
        Ok(model
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .map(str::to_string))
    }

    /// Retains each fate and each rule of `repo`; stops at the first failure (the reviewer logs it, never fails).
    pub fn retain(&self, repo: &str, fates: &[Fate], lessons: &[Lesson]) -> Result<(), MemoryError> {
        let identity = repo_identity(repo);
        let tag = format!("ultron:autoreview:project:{identity}");
        let mut items: Vec<(String, String)> = fates
            .iter()
            .map(|fate| {
                (
                    format!("autoreview:{identity}:{}:{}", fate.pull, fate.id),
                    format!(
                        "In {repo}, a {} {} finding of the {} pass at {}:{} was {} ({}): {}",
                        fate.level,
                        fate.category,
                        fate.kind,
                        fate.file,
                        fate.line,
                        match fate.fate {
                            FateKind::Accepted => "accepted",
                            FateKind::Rejected => "rejected",
                        },
                        fate.how,
                        fate.claim
                    ),
                )
            })
            .collect();
        for lesson in lessons {
            let total = lesson.accepted + lesson.rejected;
            items.push((
                format!("autoreview:{identity}:lesson:{}:{}", lesson.category, lesson.kind),
                format!(
                    "In {repo}, {} findings of the {} pass were accepted {} of {total} times in the last {FATE_DAYS} days{}",
                    lesson.category,
                    lesson.kind,
                    lesson.accepted,
                    if lesson.lowered {
                        "; the reviewer now posts such findings as low, non-blocking notes."
                    } else {
                        "."
                    }
                ),
            ));
        }
        let signal = Signal::new();
        for (document_id, content) in items {
            let operation =
                jsrt::hash::sha256_hex(format!("{document_id}:{}", (self.now)()).as_bytes())[..32].to_string();
            let request = json!({
                "async": true,
                "operation_id": operation,
                "items": [{
                    "content": content,
                    "document_id": document_id,
                    "tags": [tag],
                    "observation_scopes": [[tag]],
                    "update_mode": "replace",
                    "metadata": {"ultron_operation": operation, "ultron_evidence_class": "tool_evidence"},
                }],
            });
            self.backend.retain_items(&request, &signal)?;
        }
        Ok(())
    }
}

/// The reviewer's lesson memory (`LessonMemory` of autoreview/reviewer.ts): a failed retain is logged, never fatal.
impl crate::reviewer::LessonMemory for HindsightLessons {
    fn retain(&self, repo: &str, fates: &[Fate], lessons: &[Lesson]) -> Result<(), String> {
        HindsightLessons::retain(self, repo, fates, lessons).map_err(|error| error.to_string())
    }
    fn guide(&self, repo: &str) -> Result<Option<String>, String> {
        HindsightLessons::guide(self, repo)
    }
}
