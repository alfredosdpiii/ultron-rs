# Answer me with HTML for Ultron

`answer-me-with-html` is QingYunA's [Answer me with HTML](https://github.com/QingYunA/answer-me-with-html) skill
(version 0.6.2, commit `997479b`). The model writes a short Markdown draft and the bundled `am` CLI
(`answer-me-with-html/scripts/am.mjs`, one file with no dependencies, Node.js 20+) turns it into one HTML
page with panels, flow and sequence diagrams, trees, timelines and tables, plus a controlled-writing (STE) check. It
can also make 3Blue1Brown-style explainer videos.

One patch to `am.mjs` (search it for `Ultron:`): a page whose draft has `ultron-session: <id>` gets a **Send to
Ultron** button when it is opened through `am serve`. The button posts the reply to `/s/<page token>/<file>`, which
`am serve` only accepts from loopback with the page's token, and which runs `ultron session send <id> -` to give the
reply to that session as the user's next message. Re-apply the patch when updating `am.mjs`.

The rest of the port changes only the instructions: drafts are written to a file and rendered from a REPL cell, the CLI runs with
`AM_NO_UPDATE_CHECK=1` (this copy updates with Ultron, so it never fetches its own version file), the browser opens only
when a display is available, and the skill is read only when asked for. Pages go to `~/.answer-me-with-html/pages/`
(`AM_HOME` changes it). Video narration uses ElevenLabs only when `ELEVENLABS_API_KEY` is set.

Answer me with HTML is MIT licensed; the license is [LICENSE](LICENSE) in this directory.

Use: `/skill:answer-me-with-html <question>` (or ask Ultron to "answer with a page"). A skill named
`answer-me-with-html` in your own skills directories replaces this one.
