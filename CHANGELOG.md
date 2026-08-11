# Changelog

Written for the person installing the build. Internal churn is left out.

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
