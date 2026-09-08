# Changelog

Written for the person installing the build. Internal churn is left out.

## 0.2.15

### Changed

- **Delegation runs on Codex's own subagents now.** Ask for it in plain
  language -- "use multi agents with deepseek to investigate this project" --
  and the work goes to a real child agent thread you can open and watch, on any
  model you have enabled. LoomRouter used to run those workers itself, behind a
  single blocking call that showed nothing until it finished and gave up
  entirely at the ten minute mark. Codex's own `spawn_agent` accepts LoomRouter
  slugs, so there was never a reason to.

### Removed

- **The Agents page is gone.** Saving a persona there never affected anything:
  every delegated worker was built from the request at the moment it was
  spawned, and the saved profiles were never read. The one thing an empty
  roster did do was switch delegation off, which is the opposite of what the
  page appeared to offer. Nothing you have to do; profiles already written to
  `~/.codex/agents` stay on disk, and Codex still reads them as its own.

  The onboarding step went with it. Multi-agent stays a switch under Codex
  Integration, and delegation through LoomRouter models works whether it is on
  or off.

### Fixed

- **Models from a new Codex release finally show up.** Once the integration was
  applied, LoomRouter asked Codex for its catalog and Codex answered with the
  file LoomRouter had written -- so every refresh re-read its own output and the
  native list could never grow. It now also reads the catalog compiled into the
  Codex binary, which no round trip can echo. On a current install that is
  gpt-6-astra and the two Daybreak models appearing for the first time.

- **Claude Code shows every model your subscription serves.** The list was
  five entries fixed at build time, so Sonnet 5 and Fable 5.1 could not appear
  however often you pressed Fetch models. It now comes from the same public
  catalog LoomRouter already downloads for context windows. New models arrive
  switched off, as everywhere else. If that catalog is unreachable you keep the
  list you had.

## 0.2.14

### Added

- **New models show up on their own.** LoomRouter now re-reads every enabled
  provider's catalog, and Codex's own native list, every fifteen minutes. A
  model your provider started serving since the last time you pressed Fetch
  models appears in the providers panel by itself, switched off, with its
  context window already filled in. Turning it on stays your call, and stays
  the moment LoomRouter checks which wire dialect the model actually answers
  on, so a first real turn is never routed on a guess.

- **OrcaRouter is a preset.** Pick it from the provider list instead of typing
  the base URL by hand. It seeds no models: the 200 or so it serves arrive
  through discovery. Prompt caching is reported as unavailable, because the
  gateway accepts the cache header but never reports a cached token back.

### Fixed

- **The desktop app no longer freezes while loading its config.** The merged
  catalog was built from `codex debug models`, which returns a trimmed subset
  of the schema Codex Desktop expects. Without the missing fields the app hung
  on config load, while the LoomRouter panel cheerfully reported the
  integration as active. The catalog now carries Codex's full model schema.

- **Delegated work can write again.** The subagent server was started without
  a sandbox profile, so Codex fell back to read-only and every worker, tester,
  debugger, migrator and refactorer task failed at its first write, silently.
  It now inherits the sandbox mode from your own Codex config. If that says
  `read-only`, it is respected rather than quietly widened.

- **Claude progress appears while the turn is still running.** Progress
  reached Codex as one open item, so a long subagent run showed nothing but
  Thinking until the entire turn finished. Each step now closes as its own
  item and surfaces as it happens.

- **`deepseek-v4-flash` works through OpenCode Zen.** Every request came back
  as HTTP 502. That tier is served only over chat completions, while the Go
  gateway keeps the Responses API for the same model id, so the two presets no
  longer share a single dialect.

## 0.2.13

### Added

- **Routed Claude turns show their work.** A turn that runs through Claude Code
  used to sit silent until its answer arrived. Codex now shows the tools being
  called and the files being touched as reasoning progress, subagents announced
  by the task they were given and reported when they finish, and Claude's own
  narration as it is written rather than after the fact. Alongside it: how much
  of the 5-hour and 7-day usage windows are spent, denied permissions, and a
  closing line with the duration, turn count, tokens, cost and how many
  subagents completed.

  Nothing private travels with it. Prompts, raw tool results, the model's
  chain of thought and its signatures stay out, and what is shown is redacted
  first — a key quoted back in Claude's narration is replaced even when it
  arrives split across two pieces of the stream.

### Fixed

- **Checking for updates from the tray works.** The menu item registered its
  listener inside a chain that handled no failure, so if that registration
  ever failed the entry did nothing at all — no error, no response. It now
  reports instead of disappearing.

- **Windows auto-update from 0.2.12 is repaired.** The 0.2.12 release published
  an update manifest whose Windows entries pointed at files that were not
  there, so an installed copy asking for the update got nothing. The published
  manifest has been corrected, and the release build no longer replaces the
  assets it has already indexed.

### Changed

- **The interface is built on Tailwind 4.** Almost every pixel is unchanged;
  the exceptions are the coloured tags and status pills, which follow the new
  default palette and read very slightly more vivid, and three descriptions on
  the Codex screen that now sit at the bottom of their cards, where they were
  always meant to.

## 0.2.12

### Added

- **LoomRouter can keep the computer awake while a model is working.** A new
  setting on the Server screen chooses when idle sleep is prevented: during
  model activity, the whole time Loom is on, or never. Activity mode covers
  in-flight requests, open realtime WebSocket sessions, and the 15 minutes
  after the last one finishes. The display is never forced on in any mode.

  **This is on by default, including on upgrade.** Existing installs pick up
  "During model activity" because the setting is absent from their saved
  configuration. Pick "Never" on the Server screen to restore the previous
  behaviour, where a long routed turn could be cut short by the machine
  going to sleep.

- **The interface can follow the system theme, or be pinned light or dark.**
  A theme control sits in the sidebar footer next to the language selector
  and cycles through system, light and dark. System is the default and tracks
  the desktop's own setting, including when that setting changes while
  LoomRouter is sitting in the tray. The choice is remembered between runs.

- **MiniMax can be used directly, without a gateway in between.** A MiniMax
  key alone now puts M3 and M2.7 in the agent's picker. There are two presets
  because MiniMax splits by account region rather than by plan: a Global key
  is rejected by the mainland endpoint and vice versa, so pick the one
  matching where the account was created.

- **Native Codex models can be given a larger context window.** Each native
  model's window can be set independently — Sol up to 1,000,000 tokens, for
  instance — and put back to whatever the Codex catalog publishes. The
  setting survives restarts, and changing it while the integration is active
  reapplies it immediately. This only changes the catalog LoomRouter hands to
  Codex locally; the provider still decides what it will actually accept.

## 0.2.11

### Added

- **The tray menu can check for updates on demand.** "Check for Updates"
  opens the app and reports what it found — a pending version, that the build
  is current, or why the check failed. Downloading and installing still wait
  for explicit confirmation. Available in English, Portuguese, Spanish and
  Chinese.

- **Codex image generation and editing reach the model.** These calls
  previously hit no route at all and came back as a 404 before ever leaving
  LoomRouter; they are now forwarded to the native image backend.

- **Codex status reports whether the integration is really usable.** It now
  says whether Codex's config file parses and whether a local login session
  exists and is still valid, rather than only whether the CLI is installed.
  The token itself is never shown.

### Fixed

- **Claude Code turns can edit files and use tools again.** Routed turns run
  through `claude -p`, which has no way to ask for approval mid-run, so the
  model reported that it could not write files or run commands. Turns now
  carry an explicit permission mode, defaulting to accepting edits, and the
  selected workspace is marked trusted beforehand. WebSearch, WebFetch and
  curl are permitted.

- **Claude turns find the tools on your PATH when the app is opened from
  Finder.** A Finder launch starts with a minimal environment, so `bun`,
  `cargo` and anything else installed through a shell profile were invisible
  to routed turns. The login shell's PATH is now recovered for them.

- **Patching Codex's config no longer disturbs settings LoomRouter did not
  write.** Codex Desktop can rewrite `~/.codex/config.toml` and move or drop
  the managed markers around LoomRouter's block. Foreign tables are now
  lifted clear of that block, and the previous values are kept so a failed
  patch can be rolled back.

- **Tool definitions that strict providers rejected now go through.** Schemas
  with a union at the root, and arguments carrying whole numbers, are
  normalized before they are sent upstream.

## 0.2.10

### Added

- **The Overview now includes usage analytics.** Request and token activity can
  be inspected over time, with model and provider breakdowns that make routing
  costs and traffic easier to understand.

- **Quota reset times and Z.AI balances are visible.** Provider status now
  includes the next quota reset where available, and Z.AI accounts expose
  their current balance alongside the existing usage information.

### Fixed

- **Claude Code proxy turns no longer pollute session history.** Routed turns
  run without session persistence, so background model calls no longer create
  resumable Claude Code sessions grouped under the app's working directory.
  They also run in Claude Code safe mode, preserving subscription login and
  built-in tools without injecting personal hooks, plugins, MCP servers,
  memory or project instructions into every routed request.

- **Existing Claude Code models correctly advertise image support.** Older
  configurations could keep a stale vision flag even though routed Claude
  turns already accept images. Capabilities are now refreshed from the curated
  Claude Code catalog whenever LoomRouter loads the configuration.

- **Routed subagents cannot escalate their sandbox permissions.** A worker can
  no longer request broader filesystem access than the parent session permits.
  Analysis roles stay read-only, while workers, testers, debuggers, migrators
  and refactorers can edit inside the inherited workspace when the parent
  session already allows it. Up to eight independent routed agents can run in
  one wave, and each result shows its role, model, sandbox, duration and status.

## 0.2.9

### Fixed

- **Automatic compaction no longer fails with HTTP 413 on large Codex chats.**
  Responses and compaction requests now accept transcripts up to 128 MiB by
  default, with a configurable limit for unusually large sessions and a 1 GiB
  hard ceiling against accidental local memory exhaustion. The larger limit is
  restricted to `/v1/responses` and `/v1/responses/compact`; ordinary chat
  completions keep their existing 16 MiB limit.

## 0.2.8

### Added

- **A provider can hold several API keys instead of one.** A provider carried
  exactly one key, so a rate-limited or revoked credential took the whole
  provider down with it, and there was no way to see which account was
  spending what. Providers now keep an ordered list of named keys: a request
  fails over to the next usable key, rotation is available as an opt-in, and
  usage and balance are attributed per key on the Overview. Providers that
  already had a key keep it, migrated into the new list on first launch.

- **Claude Code turns can carry images.** The `claude-code` models are marked
  vision-capable in the catalog, so nothing advertises them as image-less any
  more, and a turn containing an image is sent to `claude -p` as Anthropic
  image blocks over `stream-json` instead of being flattened into text with
  the attachment dropped.

- **Native Codex models appear in the model pickers.** The native slugs are
  read from the captured catalog and offered alongside external provider
  models, so an agent can be pinned to one of them rather than following
  whatever the chat selected. The Codex integration's active model and
  background calls pickers list them too.

- **Codex remote compaction works over routed providers.** Compaction
  envelopes are carried transparently and replayed as plain user text for
  native and routed backends alike, with oversized payloads truncated to fit
  the destination context window. An orphaned Codex managed block now
  prompts to be repaired instead of being left as it is.

- **Agents can be tagged.** Tags are free-form, keep a stable color and
  filter the agent list. The generated orchestrator skill also carries
  multi-agent operating rules - hardware budget, agent control, token split
  - and the built-in templates are named for what they do (`code_reviewer`,
  `codebase_explorer` and the rest).

### Changed

- **Onboarding, provider cards and the dashboards were reworked.** Setup is a
  two-column view; provider cards gained a search button, a 3-dot actions
  menu and a delete confirmation; Overview, Providers and Codex got loading
  skeletons, steady empty states, equal-height cards and tooltips. Copy was
  tightened across English, Portuguese, Spanish and Chinese.

- **Saving an agent turns on multi-agent when the agent needs it**, and
  reports the save instead of leaving the button silent. The always-on tray
  restart hint is gone and the Codex restart wording is softer.

### Fixed

- **Agents no longer sit on "thinking" forever after an interrupt.** A turn
  was awaited inline, so the session stopped reading client frames while it
  streamed: a cancel sent mid-turn was not seen until the turn had already
  finished, and was then discarded without a terminal event. The client's
  turn state never closed, and from that point the connection was dead -
  every later prompt on it looked like it was still thinking, with nothing
  in the request log and no upstream connection to show for it. Because all
  agents share one session, they appeared to break together, across every
  provider. Turns now stream while frames are read, and a cancel is answered
  with `response.incomplete`, the Responses API's own terminal event.

- **Compaction can succeed on a long session.** The transcript was sized with
  a `chars/3` estimate, which fit a history into a 1M window that the
  upstream then billed at over 1.25M tokens, and a multi-megabyte tool result
  acted as a wall that discarded the history behind it. The client retries
  compaction silently, so the visible symptom was an agent that simply
  stopped answering.

- **Editing a provider no longer wipes its keys.** The Edit dialog rebuilt
  the payload with an empty key list, and saving replaces that list
  wholesale, so renaming a provider destroyed every credential stored on it.

- **A single 401 no longer disables a key until restart.** The key was parked
  permanently, and a key that is never selected again can never record the
  success that would clear the flag; it now cools down for 15 minutes. A
  malformed request no longer blames the key either - a few 400s used to take
  the only key out for 25 minutes and then report the provider as having no
  enabled key, hiding the real error behind a credentials problem that did
  not exist.

- **MiniMax thinking stays out of the answer.** Its OpenAI-compatible
  endpoint embeds thinking as raw `<think>` blocks in the content unless the
  request asks for it to be split out. LoomRouter now asks, and maps the
  reasoning fields to reasoning summaries rather than visible message text.

- **`apply_patch` works again when the model is routed over the Codex
  WebSocket.** Freeform custom tools are adapted to ordinary functions for a
  Responses upstream, and the reply has to be translated back or Codex sees a
  plain function call, finds no freeform handler and aborts the tool. The
  WebSocket path skipped that translation.

- **Images returned by a tool reach the model.** They are converted for chat
  and Anthropic upstreams, and are now also picked up when they arrive in a
  tool result's `output` rather than in message content.

- **Routed HTTP failures appear in the request log again**, with the upstream
  status intact. Every non-2xx was collapsed into a single error, which left
  the callers' error handling as dead code and handed the client a 502 for a
  429 it was meant to back off from. Network failures are now reported as
  proxy errors with a cause instead of a raw transport dump, on native
  passthrough, compaction and routed providers alike, and a failed visual
  assistance call reports its status and duration.

- **Balance cards keep their order across refreshes**, instead of reshuffling
  into whatever order the requests happened to finish in.

- **The Codex CLI is found on Windows when the native installer put it
  there.** Lookup now tries `codex.cmd`, `codex.exe` and `codex` by name.
  It no longer expands a bare name through `PATHEXT`, which had also let an
  unrelated `.bat` on PATH answer for a CLI that was not installed.

## 0.2.7

### Added

- **Models without image support can now understand images through visual
  assistance.** LoomRouter detects vision capability from the model catalog
  and, when needed, asks a configured vision model to describe the image
  before continuing the original request. Models that already support images
  keep receiving them directly.

### Fixed

- **DeepSeek models on OpenCode Go can complete tool-assisted image turns
  without entering a reconnect loop.** LoomRouter now sends the portable
  Responses shape expected by the gateway, including empty side-calls,
  function tools, reasoning items, parallel calls and their outputs.
- **Tool results stay paired with their calls across stateless routed turns.**
  Internal item identifiers and ChatGPT-only metadata are removed, structured
  outputs are converted to portable text, and interleaved assistant or
  developer context no longer separates a call from its result.
- **Rejected upstream requests now log safe structural diagnostics.** Pairing,
  ordering and reasoning-shape counts identify protocol mismatches without
  exposing prompts, tool arguments, outputs or call identifiers.

## 0.2.6

### Changed

- **OpenCode Zen and OpenCode Go are one provider each, instead of three.**
  The gateway serves some models as Chat Completions, some as Anthropic
  Messages and some as Responses, and the only way to record that used to be
  a separate provider per dialect - six entries in the picker, the same key
  pasted three times per subscription. The dialect now travels with the
  model, so there is one entry per subscription and one key. Existing setups
  are folded together on first launch, keeping their key, their enabled
  models and the context windows already learned; the saved model selection
  and the Codex integration are repointed at the merged provider.

  Models found by discovery are assumed to speak whatever the provider does
  - no catalog publishes which wire a gateway serves a model on. Where that
  guess is wrong, each model on a multi-dialect provider now carries its own
  dialect picker.

### Added

- **The running version is stamped in the sidebar footer**, next to the
  language switcher. After an update installs there was no way to tell
  which build was actually in front of you short of the Windows uninstall
  list; the stamp is the binary's own version - the same number the
  updater compares against - so it cannot misreport.

### Fixed

- **Asking for a native GPT no longer reaches OpenCode instead.** The
  gateway serves models under the same names OpenAI uses - `gpt-5.5`,
  `gpt-5.4-mini`, `grok-4.5` - and a request naming one of those without a
  provider matched whichever provider happened to serve it, silently
  answering with a different model than the one asked for. Unqualified names
  go to the native backend now, unless native-slug mode is on, which is the
  setting whose whole purpose is to publish routed models under bare names.
  The gateway's own copies stay reachable under their full name.
- **Switching a thread from a routed model to a native one no longer breaks
  the next turn.** It failed with "Item with id 'rs_…' not found. Items are
  not persisted when `store` is set to false". A routed provider returns no
  item ids, so LoomRouter invents them; the agent keeps them in the thread
  and replays them, and OpenAI's backend rejects an id it never issued.
  Those ids are now removed on the way out, keeping the content of the turn
  and the pairing between a tool call and its result.
- **Routed models can edit files again: `apply_patch` works through Chat
  upstreams.** Codex ships it as a freeform tool whose schema is a grammar
  rather than a JSON object, and nothing on the Chat path knew what to do
  with that - strict providers rejected the request outright, and when one
  did answer, the edit came back in a shape Codex filed as an unknown tool
  and aborted. Both directions are translated now, including across turns,
  so the model keeps seeing its own patches and their results.
- **No more console window next to the app on Windows.** The binary was
  linked against the console subsystem, so Windows opened a terminal beside
  it on every launch and printed the proxy's log into it - closable only by
  quitting the app. Release builds are GUI binaries now; `tauri dev` still
  prints its log to the terminal.
- **OpenCode Zen and Go models report their real context window.** Five of
  the six OpenCode presets were looking the gateway up under a catalog
  name that does not exist, so the enrichment step quietly found nothing
  and every model on them kept the conservative 128K tag - 1M models like
  `deepseek-v4-pro`, `glm-5.2` and `qwen3.6-plus` included. That number is
  also what Codex plans turns against, so it compacted conversations
  roughly eight times earlier than it had to. Both gateways now resolve,
  including the models each one publishes with a different window.
- The context tag no longer renders a raw divisor. `kimi-k3` read
  "1.048576M", and every window the vendor counts in round thousands read
  low - `grok-4.5`'s 500K as "488K", `gpt-5.4-mini`'s 400K as "391K".

## 0.2.5

### Added

- **Deferred tool loading: routed models no longer receive every tool
  definition on every request.** A typical setup sent 153 tool definitions
  per call - grafana's 56, pentest-ai's 50, the multi-agent surface, and
  more - repeated on every turn, because the API is stateless. Codex now
  advertises a single `tool_search` tool; the model searches when it needs
  something and the matches arrive activated on the next request - 17
  entries per call instead of 153, measured on a live setup. The proxy
  plays the Responses backend's part of the round-trip, so MCP servers and
  multi-agent tools work through it unchanged.
- **Real context windows in the picker and in Codex's catalog.** Every
  model showed the same conservative 128K tag - up to 8x below the real
  limit - and that is the number published into Codex's catalog, so the
  agent compacted conversations far earlier than it needed to. Fetch
  models now learns the real window from the provider's own catalog when
  it publishes one, enriches from the public models.dev catalog when it
  doesn't, and remembers what it learned per model.
- **OpenCode Go presets.** The low-cost Go subscription is the same
  opencode.ai gateway under a different path, with the same dialect split
  as Zen. A Go key only gets a 401 on the Zen endpoint, so picking Zen
  with a Go key was a dead end.

### Fixed

- **Routed models can finally use MCP servers and multi-agent tools.** The
  translator only forwarded one of the five tool shapes Codex sends and
  silently dropped the rest: of the 23 tool entries in a real request, 12
  survived. Gone with the dropped ones were the entire multi-agent
  surface, apply_patch, and every configured MCP server - which is why
  this presented for months as "routed models can't use MCP" rather than
  as an error.
- **Spawned agents now receive their task.** The multi-agent toggle wrote
  a flag Codex does not read for the surface it promised, so the model had
  no spawn tool and did everything itself; and when spawning did work, the
  child agent's task arrived in a shape the translator discarded, leaving
  it with environment and instructions but nothing to do. The toggle now
  writes the flag Codex actually reads - restart Codex after flipping it,
  it reads these flags at process start - and the task body reaches the
  child.
- **Tool-using turns no longer fail against strict providers on macOS.**
  The desktop app interleaves messages around parallel tool calls in a way
  the translator used to split apart, so strict upstreams rejected every
  tool-using turn on a Mac while the same setup worked on Windows.
- **Applying or removing the Codex integration no longer dead-ends on
  installs left by older versions.** A config written before the
  managed-block markers, or one whose ending marker the Codex desktop app
  dropped, made both apply and remove fail silently. Both shapes are now
  detected by ownership and migrated, and failures surface as errors
  instead of silence.
- The step that asks your shell where Codex lives now actually asks an
  interactive shell. zsh is the macOS default and only reads `.zshrc` for
  interactive shells - `.zprofile` and `.zlogin` cover login ones - so a
  login-only probe returned nothing on the setup it was meant to rescue,
  which is where most people put their `PATH`. Measured on a machine whose
  `PATH` lives in `.zshrc`: the login-only probe found nothing, the
  interactive one found the CLI. 0.2.4 still worked there because the
  known-locations fallback caught it; this makes the shell step carry its
  weight, which is what matters when Codex is installed somewhere unusual.
  The probe is bounded by a deadline so a slow shell profile cannot hang
  the screen that waits on it.

## 0.2.4

### Fixed

- **The Codex integration would not activate on a Mac where the app was
  opened normally.** Three of the four status rows stayed red - CLI
  detected, native catalog, merged catalog - and applying the integration
  did nothing.

  An app launched from Finder or the Dock does not inherit your shell's
  `PATH`; it gets launchd's, which is `/usr/bin:/bin:/usr/sbin:/sbin` and
  contains no package manager's bin directory. Codex installs into
  `~/.local/bin`, `/opt/homebrew/bin`, `~/.bun/bin` and similar, so the CLI
  was found when the app was started from a terminal and never when it was
  double-clicked. With no CLI there is no native catalog, and with no native
  catalog there is no merged catalog, so the failures arrived together and
  looked like the integration was simply broken on that machine.

  LoomRouter now asks your login shell where Codex is, and falls back to the
  usual install locations. If yours lives somewhere unusual, set `CODEX_BIN`
  to its full path - and the status row now says so instead of failing
  silently.

## 0.2.3

### Added

- **The menu bar is now a control surface.** Pick the active model, toggle a
  provider, and switch the whole routing on or off without opening the
  window. Turning it on starts the proxy and points Codex at it as one
  operation - and rolls the proxy back if pointing Codex fails, so a failed
  toggle never leaves you half-on.
- **The Agents screen is a catalogue, not a Codex feature list.** 22 agent
  roles that recur across the coding-agent ecosystem - reviewer, planner,
  debugger, adversarial critic, migration runner, incident responder, data
  analyst and more - grouped into eight categories. Picking one writes it
  into `~/.codex/agents` as a Codex agent you can then edit.
- **Search on the Agents screen**, covering your own agents and the catalogue
  at once. It matches the category too, so "data" finds the analyst even
  though the word is nowhere in its description.
- **A step for agents in the first-run walkthrough**, which now explains
  delegation and lets multi-agent be switched on from there.

### Fixed

- **Multi-agent could only be turned on.** The prompt to enable it only
  existed while it was off, so enabling it removed the only control. There is
  now a permanent switch under Codex Integration, alongside the other
  settings written to `~/.codex/config.toml`.
- Codex settings no longer sit in a narrow column with the rest of the window
  empty, and the third card no longer strands itself on its own row at the
  default window size.
- Agent and catalogue cards in a row are the same height.

## 0.2.2

### Added

- The Overview usage panel breaks down to the **model**, with the numbers
  that actually distinguish one from another: average latency, cache hit
  ratio, tokens, cost and failures. Previously it was one run-on line per
  provider and the model was not shown at all.
- Model rows show the **context window** published to Codex. A window
  LoomRouter only guessed at is marked, so an unconfigured provider does not
  look like a 128k model.
- The log filter chip is a button: it still auto-refreshes every 5s, and now
  refreshes on click.

### Fixed

- **Requests were missing from the dashboard.** Any provider answering in a
  usage format other than the Responses one reported zero tokens and was
  silently dropped - which was most providers. Codex traffic was unaffected,
  which is why it went unnoticed.
- Long upstream errors in the log no longer blow up the row height: they are
  clamped to one line, with the full text on hover and click to expand.
- Every screen was audited for the window sizes the app actually allows
  (900px minimum, resizable). Pages had drifted to two different widths;
  dialogs taller than the window had no way to scroll, which put Save and
  Cancel out of reach at the minimum window height; and the model dropdown
  had no height limit at all, so a provider with hundreds of models opened a
  list taller than the screen.

## 0.2.1

### Fixed

- **macOS builds would not open.** The app bundle was never signed, so
  `Contents/_CodeSignature/` was missing entirely and macOS rejected it as
  damaged - removing the quarantine flag did not help, because the signature
  itself was invalid rather than merely untrusted. Builds are now ad-hoc
  signed. They are still not notarized, so the first launch shows the usual
  unidentified-developer prompt: right-click → Open once, or
  `xattr -dr com.apple.quarantine /Applications/LoomRouter.app`.
- **Credentials were written world-readable.** `~/.codex/config.toml` holds
  the local proxy token and was created at `0644`, which let any local
  process read it and spend the stored API keys through the proxy. Both it
  and `~/.loomrouter/config.json` are now owner-only.
- A first-run walkthrough: connect Codex, add a provider, set up agents.
