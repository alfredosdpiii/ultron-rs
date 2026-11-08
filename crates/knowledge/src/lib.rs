//! The knowledge graph behind review and self-improvement: one SQLite store for every repository Ultron works in.
//!
//! The code layer records what each file is made of, keyed by git blob: constructs (a `BEFORE UPDATE` trigger, a
//! `@Controller` decorator, a call to `runWithBypass`), the symbols it defines, its calls and its imports, the graph
//! an investigator queries in SQL. A blob's constructs never change,
//! so indexing a new revision only reads the blobs the store has not seen, and reviews of different revisions of
//! one repository never disturb each other: each revision's tree is its own path-to-blob table.
//!
//! The relation layer (`relations`) is read on demand: a task asks for the relations among some files (which code
//! reads which configuration key, which handler serves which route, which code touches which table, which test
//! exercises which file); plain code proposes and prunes them, the judge settles what plain code cannot, and both
//! are cached by blob.
//!
//! This crate only writes. The kernel reads the same file with Python's `sqlite3` (`rlm/knowledge_api.py`), read-only,
//! and refuses a schema version it does not know.

mod extract;
mod index;
pub mod relations;
mod store;

pub use index::{IndexStats, MAX_RELATION_FILES, Relations, index_tree, relations};
pub use store::{Note, RuleCredit, SCHEMA_VERSION, Settled, Settlement, Store};

/// The store under the agent directory, shared by reviews and the REPL's `knowledge` API.
pub fn default_path(agent_dir: &std::path::Path) -> std::path::PathBuf {
    agent_dir.join("knowledge").join("graph.db")
}
