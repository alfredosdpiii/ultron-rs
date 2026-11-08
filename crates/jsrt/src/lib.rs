//! JavaScript semantics for the TypeScript port, in one leaf crate.
//!
//! Matching what the TypeScript product printed, stored and compared is the contract of most crates here, and each
//! one had grown its own copy of the same few JavaScript primitives, some subtly wrong (Rust's `trim` drops U+0085,
//! `{:.1}` rounds ties to even, `serde_json` writes `1.0` and keeps `"10"` after `"b"`). These are the versions the
//! copies agree on when they are right, each pinned by tests whose expected values node printed.
//!
//! - [`json`]: `JSON.stringify` (compact and indented), `Object.entries` order, numbers as JSON values.
//! - [`number`]: `String(x)`, `toFixed`, `Math.round`, `Number(text)`, safe integers.
//! - [`value`]: truthiness, `String(value)` and `Number(value)` of a JSON value.
//! - [`string`]: the `\s` / `trim` whitespace set and `replace(/\s+/g, " ").trim()`.
//! - [`utf16`]: `length`, `slice` and default `sort()` order in UTF-16 code units.
//! - [`date`]: `Date.now()`, `Date.parse`, `toISOString`, Node's `mtimeMs`; [`localtime`]: local time under `TZ`.
//! - [`collate`]: `localeCompare` (ICU root collation) for the names the TUI and catalogs sort.
//! - [`base64`], [`hash`], [`random`]: `btoa`/`atob`/`Buffer.from`, sha256 hex and HMAC, `randomUUID` and UUIDv7.
//! - [`sync`]: a poison-free `Mutex::lock`; [`payload`]: the "Unknown payload field" check.
//! - [`diff`]: jsdiff's Myers diff, `diffLines` and the edit tool's `generateDiffString`.

pub mod base64;
pub mod collate;
pub mod date;
pub mod diff;
pub mod hash;
pub mod json;
pub mod localtime;
pub mod number;
pub mod payload;
pub mod random;
pub mod string;
pub mod sync;
pub mod utf16;
pub mod value;
