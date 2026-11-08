//! The session value namespaces Ultron writes into a format-4 journal, and the versioned documents among them.
//! Every crate that reads or writes one of these values names it from here.

/// `getName`/`setName` (key `""`).
pub const SESSION_NAME: &str = "ultron.session.name";
/// Entry labels, keyed by the labelled entry's id.
pub const ENTRY_LABEL: &str = "ultron.entry.label";
/// A lane's branch tip, keyed by lane.
pub const BRANCH_TIP: &str = "ultron.branch.tip";
/// A lane's `LaneConfiguration`, keyed by lane.
pub const LANE_CONFIG: &str = "ultron.lane.config";
/// A lane's state (current and last operation, inbox), keyed by lane.
pub const LANE_STATE: &str = "ultron.lane.state";

/// The harness operation journal of a run (the TypeScript drive), keyed by operation id.
pub const OP_META: &str = "ultron.op.meta";
pub const OP_STATE: &str = "ultron.op.state";
pub const OP_RESULT: &str = "ultron.result";
pub const OP_TOOL_ARGS: &str = "ultron.op.tool_args";
/// A compaction's durable preparation, keyed `<operation>:<task>`.
pub const OP_PREPARATION: &str = "ultron.op.preparation";
pub const PENDING_ENTRY: &str = "ultron.pending.entry";
pub const PENDING_TOOL_OUTPUT: &str = "ultron.pending.tool_output";
pub const PENDING_FRAMES: &str = "ultron.pending.assistant_frame";

/// Working-tree checkpoints (Rust-only), keyed by the turn's prompt entry id or `restore:<id>`.
pub const CHECKPOINTS: &str = "ultron.checkpoint";

/// The session's usage ledger (`root`).
pub const USAGE: &str = "ultron.usage";
/// The delegation task ledger (`root`).
pub const TASKS: &str = "ultron.tasks";
/// The agent definitions of the session (`root`).
pub const DEFINITIONS: &str = "ultron.definitions";
/// The memory journal (`root`).
pub const MEMORY_STATE: &str = "ultron.memory.state";
/// Ultron's own module state, keyed by module (`stats`, `skills`, `durable`, `session-name`, ...). The TypeScript
/// product keeps it opaque.
pub const MODULE: &str = "ultron.module";
/// The local services' documents, keyed by service (`ultron.refinements`, `ultron.experiments`, ...).
pub const LOCAL: &str = "ultron.local";

/// A versioned Ultron document: the value at `namespace`/`key` whose `version_field` names its format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Document {
    pub namespace: &'static str,
    pub key: &'static str,
    pub version_field: &'static str,
}

impl Document {
    const fn root(namespace: &'static str) -> Document {
        Document {
            namespace,
            key: "root",
            version_field: "version",
        }
    }

    const fn local(key: &'static str) -> Document {
        Document {
            namespace: LOCAL,
            key,
            version_field: "formatVersion",
        }
    }

    /// `<namespace>/<key>`, as errors name the document.
    pub fn address(&self) -> String {
        format!("{}/{}", self.namespace, self.key)
    }
}

pub const TASKS_ROOT: Document = Document::root(TASKS);
pub const DEFINITIONS_ROOT: Document = Document::root(DEFINITIONS);
pub const USAGE_ROOT: Document = Document::root(USAGE);
pub const MEMORY_ROOT: Document = Document::root(MEMORY_STATE);
pub const REFINEMENTS: Document = Document::local("ultron.refinements");
pub const EXPERIMENTS: Document = Document::local("ultron.experiments");
pub const ARTIFACT_REFS: Document = Document::local("ultron.artifact-refs");

/// The single-value documents a resuming session checks (format-version.ts `SINGLE_VALUES`); `ultron.module`
/// documents are checked by key ([`module_version_field`]).
pub const VERSIONED: [Document; 7] = [
    TASKS_ROOT,
    DEFINITIONS_ROOT,
    USAGE_ROOT,
    MEMORY_ROOT,
    REFINEMENTS,
    EXPERIMENTS,
    ARTIFACT_REFS,
];

/// The version field of the `ultron.module` document `key` (`moduleField`): the skills catalog names it `format`.
pub fn module_version_field(key: &str) -> &'static str {
    if key == "skills" { "format" } else { "version" }
}
