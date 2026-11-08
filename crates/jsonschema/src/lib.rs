//! One JSON-schema validator for every schema the host checks: tool parameters (the agent loop's tools, MCP and
//! extension tools called from the REPL), typed agent definitions, spawn contracts and `rlm.infer` contracts. Each is
//! checked as the TypeScript product checks it, with typebox 1.3:
//!
//! - [`validate_json_schema`]: the meta-check of definition-registry.ts, and [`unchecked_keyword`], the keywords a
//!   runtime schema may not use because they are not evaluated here.
//! - [`check`] / [`errors`]: `Value.Check` / `Value.Errors` ([`SchemaError`]: keyword, path, params, message).
//! - [`formats`]: the string formats typebox registers, and ECMAScript regular expressions.
//! - [`validate_tool_arguments`] / [`prepare_tool_arguments`]: a tool call's arguments normalized, converted and
//!   coerced to the schema, or the "Validation failed for tool" message.

mod check;
mod convert;
pub mod formats;
mod meta;
mod tool;

pub use check::{SchemaError, check, errors, grapheme_count, json_equal};
pub use meta::{unchecked_keyword, validate_json_schema};
pub use tool::{prepare_tool_arguments, validate_tool_arguments};
