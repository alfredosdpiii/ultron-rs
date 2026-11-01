# Ultron skills

Skills written for Ultron itself, as opposed to the ports in the sibling directories.

- `debug-unfamiliar`: facts-first debugging for code nobody here knows well. The agent compares the broken case
  against a control, ranks hypotheses with a hypotheses-by-facts matrix, descends one layer at a time for a strong
  signal, and accepts a cause only when it fits every fact. The method follows Cindy Sridharan's
  [How to effectively debug when you no longer understand the code](https://copyconstruct.medium.com/how-to-effectively-debug-when-you-no-longer-understand-the-code-bd3dcb2c5f7b).

- `dream`: nightly consolidation for the durable instance. The durable server keeps a daily `dream` schedule
  (`learn.dream`, default on; `learn.dreamAt`, default `03:30` local) in its own session; each run reads the day's
  sessions, autoreview's lessons and Hindsight, keeps only quoted, recurring lessons as keyed global memories or
  tested code skills, retires contradicted facts, and writes `~/.ultron/agent/dreams/<date>.md` with how to undo each
  change.

Use: `/skill:<name>`. A skill with the same name in your own skills directories replaces the bundled one.
- `verified-core`: proved specs for small pure logic. The user states laws in `LAWS.bend`, Bend proves a reference
  implementation, and the shipped code is either the compiled Bend (`-o x.mjs` for JS/TS) or tested against it as
  an oracle (any language). `example/` is a checked last-write-wins replay with a 2000-case Python differential
  test. Needs [Bend](https://bend-lang.com); falls back to Hypothesis, fast-check, proptest or CrossHair.
