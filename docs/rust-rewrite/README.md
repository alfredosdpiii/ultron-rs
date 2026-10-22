# Rust rewrite references

The execution plan is `rewriteplan.md` at the root of the `rust-rewrite` branch of
[alfredosdpiii/ultron](https://github.com/alfredosdpiii/ultron/blob/rust-rewrite/rewriteplan.md).

- [`baseline.json`](baseline.json): pinned source revisions, package census, and A01–A56 reference evidence. Rust coverage is not implemented.
- [`decisions.tsv`](decisions.tsv): append-only decision history. Earlier links to sections in this directory's README refer to that plan.

The approved architecture is a Rust host and durable engine with a persistent Python REPL.
