# Settings Reference

This reference lists user-configurable settings, their types, defaults, and purposes. Project settings override agent-directory settings. Resource lists are combined. See [Configuration](configuration.md) for file locations and trust behavior.

## Model and thinking

<a id="model-cycling"></a>

| Setting | Type | Default | Description |
|---|---|---|---|
| `defaultProvider` | string | Automatic | Startup AI provider. |
| `defaultModel` | string | Automatic | Startup model ID. |
| `defaultThinkingLevel` | `"off" \| "minimal" \| "low" \| "medium" \| "high" \| "xhigh" \| "max"` | `"medium"` | Startup thinking level. |
| `modelThinkingLevels` | object | None | Per-model startup thinking levels keyed by exact `provider/modelId`. |
| `thinkingBudgets` | object | Built-in budgets | Token budgets for `minimal`, `low`, `medium`, and `high` thinking levels. |
| `enabledModels` | `string[]` | All available models | Model patterns used for startup selection and model cycling. Uses the same format as `--models`. |
| `hideThinkingBlock` | boolean | `false` | Hide thinking blocks in the transcript. |
| `showCacheMissNotices` | boolean | `false` | Show notices for significant cache misses, successful cache warming, compaction usage, and provider recovery. |
| `cacheWarming` | `"off" \| "streaming" \| "idle"` | `"streaming"` | Keep eligible provider prompt caches warm during active runs or, with `"idle"`, between runs. Global setting only. |

Cache warming runs only when the model declares a cache lifetime and Pi estimates at least $0.05 in avoided cache-miss cost. Refresh usage counts toward session totals but does not enter model context. `/session` shows the next decision; extensions can override it with `cache_warming_decision`. See [Prompt Cache Lifetimes](models.md#prompt-cache-lifetimes).

See [Choose a Model](models.md) for model selection and thinking controls.

### Models of RLM work (Ultron)

Global settings only (`~/.ultron/agent/settings.json`); a project's `settings.json` cannot set them, so a repository
cannot route your frames or sub-agents to another provider. In the native TUI, `/settings → Models` shows each one
with its effective value and where it comes from (setting, environment variable, default or the session's model) and
sets it with a picker over the models your providers make available. Changes apply to the running session: the
worker reads them for every frame, `/review` and spawn, so the next one uses the new value without a restart. A row an
environment variable overrides is shown locked with the variable's name.

| Setting | Type | Default | Description |
|---|---|---|---|
| `rlm.frameModel` | `provider/model` | The session's model | Code-free `rlm.infer`/`rlm.map` frames that name no `model=` (and `/review` frames without `review.model`). Order: `model=` > `ULTRON_RLM_FRAME_MODEL` > this > the session's model. A frame with depth (it runs the REPL) keeps the session's model. `claude-code/*` models are allowed. |
| `review.model` | `provider/model` | `rlm.frameModel` | `/review` finder and verifier frames. Order: `--model` > `ULTRON_REVIEW_MODEL` > this > the frame model. `--deep` sub-agents do not use it (they need tools). |
| `rlm.childModel` | `provider/model` | The session's model | `rlm.spawn` sub-agents that name no `model=`. Must be tool-capable: `claude-code/*` is refused (and ignored if hand-edited in). Under `ultron claude`, `claudeCode.ultronChildModel` comes first. |
| `rlm.frameThinking` | `"off" \| "minimal" \| "low" \| "medium" \| "high" \| "xhigh" \| "max"` | The session's level | Thinking level of inference frames (`claude-code` frames pass it as `--effort`). `/settings` offers off, low, medium and high. |
| `claudeCode.model` | Claude Code alias | `claude-opus-5-5` | The model Claude Code itself runs under `ultron claude` (`claude --model`). `--model` and `ULTRON_CLAUDE_MODEL` override it. |
| `claudeCode.frameModel` | `provider/model` | `claude-code/claude-opus-5-5` | Frames under `ultron claude`. `--frame-model` and `ULTRON_CLAUDE_FRAME_MODEL` override it. |
| `claudeCode.childModel` | Claude Code alias | `claude-opus-5-5` | Claude Code sub-agents (`children: "claude"`). `--child-model` and `ULTRON_CLAUDE_CHILD_MODEL` override it. |
| `claudeCode.ultronChildModel` | `provider/model` | The default model | Ultron sub-agents under `ultron claude` (`children: "ultron"`); tool-capable models only. |

`ultron autoreview` has its own `autoreview.*` settings (accounts, models, budget, dry run, acknowledgement); see
[autoreview.md](autoreview.md#settings).

The session's own model is `defaultProvider`/`defaultModel`: the Models section's "Session model" row switches the
running session and saves it as the default, as `/model` does. The `claudeCode.*` rows apply when `ultron claude` next
starts.

### Token caps (Ultron)

Both are off unless set; `/settings` lists them as "Request token cap" and "rlm.map token cap" (off, or a few sizes).

| Setting | Type | Default | Description |
|---|---|---|---|
| `rootBudget.maxTotalTokens` | positive integer \| `"off"` | off | Tokens one request (a root turn, with its sub-agents and frames) may spend before more model work is refused. Read when a session starts. `ULTRON_MAX_TOTAL_TOKENS` (a count, or `off`) overrides it. |
| `rlm.mapTokens` | positive integer \| `"off"` | off | Token budget of an `rlm.map` that sets no `budget=Budget(tokens=...)`; frames past it come back `Incomplete`. Applies to the next map. `ULTRON_RLM_MAP_TOKENS` overrides it. |

### REPL result footer (Ultron)

Global only. `/settings` shows it as "REPL result footer".

| Setting | Type | Default | Description |
|---|---|---|---|
| `rlm.resultFooter` | boolean | `false` | Each rlm result ends with `[t+<time since the session started> ctx <tokens of the request that ran it>]`, and the runtime guide tells the model so. Applies to new sessions; not with the TypeScript guide (`ULTRON_GUIDE=ts`). |
| `rootBudget.maxCalls.task` | positive integer | none | Sub-agent tasks one request may admit before more are refused. Read when a session starts. `ULTRON_MAX_TASK_CALLS` (a count, or `off`) overrides it. Not in `/settings`. |
| `rootBudget.maxCalls.model` | positive integer | none | Model calls one request may admit. Read when a session starts. `ULTRON_MAX_MODEL_CALLS` (a count, or `off`) overrides it. Not in `/settings`. |

### Judge seat (Ultron)

`judge.decide` answers closed questions (yes/no, a choice, a number) with a small judge model. Global settings only:
they point at services and spend money. `ultron setup` sets up the backends and their keys (`TYPESAFE_API_KEY` or
auth.json `judge:typesafe` for Jev, `ULTRON_D1_KEY` or auth.json `judge:d1` for d1); keys are never settings. A key
saved in auth.json is bound to the server it was saved for (its `origin`): pointed at another server, it is not
sent, and a key goes over plain http only to this machine (localhost, 127.0.0.0/8, ::1). Before any backend is asked,
secrets in the text it judges are redacted with the rules cell output is masked with. `/settings` shows
"Judge backends", "Judge budget" and, under Models, the fallback and escalation models; every key below is applied
by the session worker and read at each `judge.decide`.

| Setting | Type | Default | Description |
|---|---|---|---|
| `judge.provider` | `("jev" \| "d1")[]` | `["jev", "d1"]` | Backends tried in order; a backend without a key, an error, a timeout or an unknown cost moves to the next. `[]` turns them off. |
| `judge.d1.url` | URL | none | The d1 server (`<url>/decide`); unset, d1 is not configured. https, or http to this machine only. |
| `judge.model` | string | `"jev-latest"` | The Jev model asked for. |
| `judge.fallbackModel` | `provider/model` | none | Asked when no backend answers; unset, the verdict abstains. |
| `judge.escalateModel` | `provider/model` | none | Answers under their confidence threshold go to it; it must be of another model family than the one that answered. Unset, they come back for review. |
| `judge.thresholds` | object | Jev 0.75, d1 0.85 | Confidence thresholds from 0 to 1: `{<backend>: n}`, and `{<kind>: {<backend>: n}}` for one question kind. `ultron optimize judge` fits them into a proposal file you apply by hand. |
| `judge.budgetUsd` | number | none | USD one session's judge may spend (Jev and the chat models; d1 is free). Unset, no cap; spend is still recorded. |
| `judge.maxCalls` | positive integer | 1000 | Backend calls one session's judge may make. |
| `judge.jev.pricePerMTok` | number | none | USD per million tokens of a Jev answer that reports no cost; unset, such an answer has an unknown cost. |

### Memory backend (Ultron)

Global settings only (they point at a service). `ultron setup` has a step for them, and `/settings` shows "Memory
backend".

| Setting | Type | Default | Description |
|---|---|---|---|
| `memory.backend` | `"memorantado" \| "hindsight"` | detected | Where `memory.*`, automatic memory and dreaming keep memories. Unset, memorantado is used once its server answers when a session starts, and that choice is kept (`<agent dir>/memory-backend`); until then, Hindsight. A Hindsight server you set (`hindsightUrl`, `ULTRON_HINDSIGHT_URL`) is kept while memorantado was never chosen: nothing is detected. Once memorantado is the backend, set or detected, a session where it does not answer pauses memory with a notice and writes nothing to Hindsight, so memories never split across two stores; `hindsight` switches back explicitly. Applies to new sessions. |
| `memorantadoUrl` | URL | `http://127.0.0.1:3789` | The memorantado server. `ULTRON_MEMORANTADO_URL` overrides it; `off` turns memorantado off. |

`ultron memory migrate --to memorantado` copies Hindsight's memories into memorantado once.

### Knowledge graph mirror (Ultron)

Global settings only. When `graph.neo4j.url` is set, the relations `knowledge.relations` accepts, and, when memory
lives in memorantado (set or detected), memorantado's memories with their `supersedes` and conflict links, are
mirrored to Neo4j. The knowledge store stays the source of truth; a mirror that fails is reported and never fails the
call. The password is `NEO4J_PASSWORD`, else auth.json `graph:neo4j` (`{"type": "api_key", "key": ..., "origin":
"https://<host>[:port]"}`: it is sent only to that server); never a setting. Not in `/settings`.

| Setting | Type | Default | Description |
|---|---|---|---|
| `graph.neo4j.url` | URL | none | Neo4j's HTTP endpoint (`<url>/db/<database>/query/v2`); unset, no mirror. https, or http to this machine only. |
| `graph.neo4j.database` | string | `"neo4j"` | The database. |
| `graph.neo4j.user` | string | `"neo4j"` | The user. |

## Interaction

| Setting | Type | Default | Description |
|---|---|---|---|
| `steeringMode` | `"all" \| "one-at-a-time"` | `"one-at-a-time"` | How queued steering messages are delivered. |
| `followUpMode` | `"all" \| "one-at-a-time"` | `"one-at-a-time"` | How queued follow-up messages are delivered. |
| `externalEditor` | string | `$VISUAL`, `$EDITOR`, then platform default | Command opened by the external-editor keybinding. |
| `doubleEscapeAction` | `"tree" \| "fork" \| "none"` | `"tree"` | Action for double Escape with an empty editor. |
| `treeFilterMode` | `"default" \| "no-tools" \| "user-only" \| "labeled-only" \| "all"` | `"default"` | Initial filter used by `/tree`. |
| `defaultProjectTrust` | `"ask" \| "always" \| "never"` | `"ask"` | Fallback project-trust behavior. **Can only be set in agent-directory settings.** |

## Tools

| Setting | Type | Default | Description |
|---|---|---|---|
| `defaultTools` | `string[]` | `read`, `bash`, `edit`, `write` | Built-in tools enabled at startup. An empty array disables all built-in tools but not extension or SDK tools. |

Available built-in tools are `read`, `bash`, `powershell`, `edit`, `write`, `grep`, `find`, and `ls`. CLI tool options override this setting for one invocation. See [Command Line](cli.md#tools).

### Loki guardrails (Ultron)

Global settings only (`~/.ultron/agent/settings.json`), so a repository cannot switch its own checks off. `ultron setup` edits them.

| Setting | Type | Default | Description |
|---|---|---|---|
| `loki.mode` | `"on" \| "advise" \| "off"` | `"on"` | Check every REPL file write with the bundled Loki engine: `on` blocks unsafe `edit()`/`write()` calls, `advise` only reports, `off` disables Loki. `ULTRON_LOKI` overrides it. |
| `loki.autoInit` | boolean | `true` | Create `.loki/` (engine and default policy, nothing else) in a Git repository that lacks it. Never in CI, a repository rooted at `$HOME` or `/`, under node_modules or in a read-only checkout. `ULTRON_LOKI_AUTOINIT` overrides it. |
| `loki.autoCommit` | boolean | `true` | Commit the `.loki/` Ultron created, and only it, as "Add Loki guardrails" with your Git identity; never pushed, hooks and signing respected. On a fork this commit rides along in pull requests. `ULTRON_LOKI_AUTOCOMMIT` overrides it. |
| `loki.ignoreRepos` | `string[]` | `[]` | Repository roots (absolute or `~/...`) where `.loki/` is never created. |

`ULTRON_LOKI_TIMEOUT_MS` (default 5000) bounds each before-write check; a check that takes longer lets the write through with a visible note. `ULTRON_LOKI_LOG=<file>` appends every check as a JSON line.

### Language servers (Ultron)

| Setting | Type | Default | Description |
|---|---|---|---|
| `lsp.enabled` | boolean | `true` | Show the errors a language server finds in the new text of every REPL `edit()`/`write()`, as a note on that write; the write is never refused. `ULTRON_LSP` overrides it. Always off while `sandbox.mode` is not `off`, because the servers run outside the sandbox. |

| Server | Files | Runs when | mise installs |
|---|---|---|---|
| rust-analyzer | `.rs` | the file is in a Cargo project | `rust-analyzer` |
| pyright | `.py`, `.pyi` | always | `npm:pyright` |
| typescript-language-server | `.ts`, `.tsx`, `.mts`, `.cts`, `.js`, `.jsx`, `.mjs`, `.cjs` | always | `npm:typescript-language-server`, `npm:typescript` |
| effect-tsgo | the same | instead of typescript-language-server, when the project's `tsconfig.json` names `@effect/language-service` and its own TypeScript is not 5 or 6 | `npm:@effect/tsgo` |
| gopls | `.go` | the file is in a Go module | `go:golang.org/x/tools/gopls` |
| clangd | `.c`, `.h`, `.cc`, `.cpp`, `.cxx`, `.hh`, `.hpp`, `.hxx` | the project has `compile_commands.json`, `compile_flags.txt` or `.clangd` | `github:clangd/clangd` |
| zls | `.zig` | always | `zls` |
| lua-language-server | `.lua` | always | `lua-language-server` |
| terraform-ls | `.tf`, `.tfvars` | always | `terraform-ls` |
| bash-language-server | `.sh`, `.bash` | always | `npm:bash-language-server` |
| yaml-language-server | `.yaml`, `.yml` | always | `npm:yaml-language-server` |
| taplo | `.toml` | always | `taplo` |

"Always" means in a Git repository or a directory tree with the server's project file; a file outside both is not checked. A server starts with the first write to a file of its project and is shared by the session's lanes. One that is missing from `PATH`, or is there and does not start, is run with `mise exec` when mise is installed, which installs it first; that install can take minutes, and the writes made meanwhile are not checked. Only errors are shown, at most ten per write. A write waits at most 4 seconds for them.

Effect: TypeScript 5 and 6 load the `@effect/language-service` plugin in typescript-language-server when the project has it installed and named in `tsconfig.json`. TypeScript 7 loads no plugins, so such a project is checked by `effect-tsgo` (TypeScript 7 with the Effect diagnostics built in).

rust-analyzer runs without `cargo check`, build scripts and proc macros, so it never builds anything. It reports syntax errors, type mismatches, wrong argument counts, missing fields and unresolved names. Errors it can only guess at without macro expansion are not shown: `E0277`, `E0282`, `E0432`, `E0599`, errors inside macro expansions, and unresolved names in a file with a macro it could not expand. zls writes a `.zig-cache` directory into the project.

## Sessions and context

| Setting | Type | Default | Description |
|---|---|---|---|
| `sessionDir` | string | Agent session directory | Session storage directory. Relative paths resolve from the working directory. `PI_CODING_AGENT_SESSION_DIR` and `--session-dir` override this setting. |

### Compaction

| Setting | Type | Default | Description |
|---|---|---|---|
| `compaction.enabled` | boolean | `true` | Enable automatic compaction. |
| `compaction.reserveTokens` | number | `16384` | Tokens reserved for the model response. |
| `compaction.keepRecentTokens` | number | `20000` | Recent tokens retained without summarization. |
| `compaction.modelOverrides` | object | None | Per-model token settings keyed by exact `provider/modelId`. |

<a id="per-model-compaction-overrides"></a>

Compaction token values must be non-negative safe integers. Each value resolves independently from the matching model override, then the ordinary compaction setting, then the built-in default. Project and user objects merge before model lookup.

See [Compaction Reference](compaction.md) for trigger, summarization, and validation behavior.

### Branch summaries

| Setting | Type | Default | Description |
|---|---|---|---|
| `branchSummary.reserveTokens` | number | `16384` | Tokens reserved when summarizing branch history. |
| `branchSummary.skipPrompt` | boolean | `false` | Skip the branch-summary prompt and default to no summary. |

## Terminal and display

| Setting | Type | Default | Description |
|---|---|---|---|
| `theme` | string | Detected | Built-in or custom theme name. |
| `quietStartup` | boolean | `false` | Hide the startup header. |
| `rlmPaneAutoOpen` | boolean | `true` | Ultron's native TUI: open the RLM pane beside the chat, without the focus, when RLM work it shows starts (a child, typed agent task, `workflows.run` node or `rlm.map`), on terminals of 120 columns or more. `ULTRON_RLM_PANE_AUTO=off` also turns it off. |
| `tuiMode` | `"regular" \| "fullscreen"` | `"regular"` | Interactive terminal UI mode. |
| `fullscreenExitOutput` | `"transcript" \| "resume-hint"` | `"transcript"` | Output printed when fullscreen mode exits. |
| `fullscreenScrollbar` | `"auto" \| "always" \| "hidden"` | `"auto"` | Fullscreen transcript scrollbar behavior. |
| `fullscreenCopyOnSelect` | boolean | `true` | Copy selected text automatically in fullscreen mode. |
| `editorPaddingX` | number | `0` | Horizontal editor padding from 0 to 3 cells. |
| `outputPad` | `0 \| 1` | `1` | Horizontal transcript padding. |
| `autocompleteMaxVisible` | number | `5` | Visible autocomplete entries, from 3 to 20. |
| `showHardwareCursor` | boolean | `false` | Show the terminal cursor while Pi positions it for input methods. |
| `terminal.showImages` | boolean | `true` | Display inline images when supported. |
| `terminal.imageWidthCells` | number | `60` | Preferred inline image width in terminal cells. |
| `terminal.clearOnShrink` | boolean | `false` | Clear empty rows when rendered content shrinks. |
| `terminal.showTerminalProgress` | boolean | `false` | Show OSC 9;4 progress in the terminal tab. |
| `terminal.hyperlinks` | `boolean \| "auto"` | `"auto"` | Override OSC 8 hyperlink detection. |
| `terminal.images` | `"kitty" \| "iterm2" \| "auto" \| false` | `"auto"` | Override inline-image protocol detection. |
| `terminal.trueColor` | `boolean \| "auto"` | `"auto"` | Override true-color detection. |
| `images.autoResize` | boolean | `true` | Resize images to at most 2000 by 2000 pixels before sending them to a model. |
| `images.blockImages` | boolean | `false` | Prevent images from being sent to models. |
| `markdown.codeBlockIndent` | string | `"  "` | Prefix used to indent rendered code blocks. |
| `markdown.mermaid` | `"off" \| "final" \| "streaming"` | `"streaming"` | Mermaid rendering mode. |

See [Themes](themes.md) and [Terminal Setup](terminal-setup.md) for format and platform details.

## Network and retries

| Setting | Type | Default | Description |
|---|---|---|---|
| `transport` | `"auto" \| "sse" \| "websocket" \| "websocket-cached"` | `"auto"` | Preferred transport for AI providers that support multiple transports. |
| `httpProxy` | string | None | Proxy URL applied as `HTTP_PROXY` and `HTTPS_PROXY` for Pi-managed HTTP clients. **Can only be set in agent-directory settings.** |
| `httpIdleTimeoutMs` | number | `300000` | HTTP header and body idle timeout in milliseconds. Set to `0` to disable. |
| `websocketConnectTimeoutMs` | number | `15000` | WebSocket connection timeout in milliseconds. Set to `0` to disable. |
| `retry.enabled` | boolean | `true` | Enable automatic agent-level retry for transient failures. |
| `retry.maxRetries` | number | `3` | Maximum agent-level retry attempts. |
| `retry.baseDelayMs` | number | `2000` | Initial exponential-backoff delay in milliseconds. |
| `retry.maxAgentDelayMs` | number | `60000` | Maximum agent-level retry delay in milliseconds. |
| `retry.provider.timeoutMs` | number | `httpIdleTimeoutMs` | Provider request timeout in milliseconds. |
| `retry.provider.maxRetries` | number | `0` | Provider-level retry attempts. |
| `retry.provider.maxRetryDelayMs` | number | `60000` | Maximum server-requested delay in milliseconds. Set to `0` to disable the limit. |

Keep `retry.provider.maxRetries` at `0` unless provider-level retries are required. Provider retries can delay Pi from handling quota and usage-limit errors itself.

## Shell

| Setting | Type | Default | Description |
|---|---|---|---|
| `shellPath` | string | Platform default | Custom shell executable path. Supports a leading `~`. |
| `shellCommandPrefix` | string | None | Prefix prepended to every shell command. |
| `npmCommand` | `string[]` | `npm` | Command and arguments used for npm package lookup and installation. |

See [Shell aliases](shell-aliases.md) for shell setup and [Pi Packages](packages.md) for package-manager behavior.

## Resources

Resource paths in user settings resolve from the agent directory. Paths in project settings resolve from the project `.pi` directory. Absolute paths and `~` are supported.

| Setting | Type | Default | Description |
|---|---|---|---|
| `packages` | array | `[]` | npm, git, or local Pi package sources. See [Pi Packages](packages.md). |
| `extensions` | `string[]` | `[]` | Extension files or directories. |
| `skills` | `string[]` | `[]` | Skill files or directories. |
| `prompts` | `string[]` | `[]` | Prompt-template files or directories. |
| `themes` | `string[]` | `[]` | Theme files or directories. |
| `enableSkillCommands` | boolean | `true` | Register skills as `/skill:name` commands. |
| `alwaysSkills` | `string[]` | `[]` | Skill names whose full text is in the system prompt while engineering mode is on, so they apply to every task without being read or invoked (16,000 characters in all). See [Skills](skills.md). |
| `engineering` | boolean | `false` | Engineering mode: puts the `alwaysSkills` in the system prompt. `/engineering true\|false` sets it. |
| `selfReview` | `"require"` \| `"advise"` \| `"off"` | `"require"` | Engineering mode: what shipping asks. `advise` refuses a lane's first commit, push or `gh pr create` once, with the `blast-radius` skill. `require` also refuses `gh pr create` until `/review` has reviewed the committed tree and no confirmed finding is left unwaived, and until its `--body-file` is the description `spec_api.description()` wrote for that tree. See [Skills](skills.md#self-review). |
| `situationalSkills` | `object[]` \| `false` | built-in rules | Engineering mode: a cell that changes a file matching a rule's `paths` ends its result with the rule's `skill`, once per session. `false` or `[]` turns it off. See [Skills](skills.md#situational-skills). |

Resource arrays support glob exclusions with `!pattern`, exact inclusion with `+path`, and exact exclusion with `-path`. Pi loads resources listed in both user-level and project settings.

## Updates, telemetry, and warnings

| Setting | Type | Default | Description |
|---|---|---|---|
| `collapseChangelog` | boolean | `false` | Show a condensed changelog after an update. |
| `enableInstallTelemetry` | boolean | `true` | Send selected provider attribution headers (OpenRouter, NVIDIA NIM, Cloudflare). Ultron does not report installs or updates. Does not control update checks. |
| `enableAnalytics` | boolean | `false` | Opt in to analytics data sharing. Currently used only by the experimental first-run setup. |
| `warnings.anthropicExtraUsage` | boolean | `true` | Warn when Anthropic subscription authentication may use paid extra usage. |
