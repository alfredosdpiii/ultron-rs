//! The REPL's `knowledge` API and the knowledge graph's optional mirror.
//!
//! `knowledge.relations(...)` reads relations across files for a task (crates/knowledge `relations`): plain code
//! proposes and prunes, the REPL client asks the judge about what plain code could not settle, and the verdicts
//! are cached in the knowledge store by blob ([`module`]).
//!
//! SQLite stays authoritative. When `graph.neo4j.url` is set, accepted relations are mirrored to Neo4j through its
//! HTTP Query API ([`neo4j`]), and, when memory lives in memorantado, its memories and their `supersedes` and
//! conflict links are mirrored read-only beside them (read by memorantado's own client:
//! `memorantado::MemorantadoBackend::project_graph`). A mirror that fails is reported in the reply and never fails
//! the request.

pub mod module;
pub mod neo4j;

pub use module::{IssuedReceipts, MODEL_CHECKED};
pub use module::{KnowledgeModule, Mirrors};
pub use neo4j::Neo4j;
