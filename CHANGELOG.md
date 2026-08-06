# Changelog

Written for the person installing the build. Internal churn is left out.

## 0.2.5

### Fixed

- **Tool calls stopped working with strict upstreams (OpenCode Go / Console Go / DeepSeek) on the Codex desktop app.** Two translation bugs combined into one failing turn: parallel tool calls were split into separate assistant messages, and Codex's macOS app interleaves a `developer` (system) message between the assistant's tool calls and their results. Either way the request reached the upstream with a `tool_calls` message that was never immediately answered, and the API rejected it with "an assistant message with 'tool_calls' must be followed by tool messages responding to each 'tool_call_id'." Every engineering prompt that reached for a tool failed on a Mac while the same setup worked on Windows, because the desktop client sends that interleaved item and the CLI does not.

  Parallel calls now share a single assistant message, and interleaved system messages are hoisted in front of the `tool_calls` message so the tool sequence stays contiguous. Plain chat was never affected, which is why the failure only showed up once tools were in play.

- **Routed models could not use multi-agent or MCP tools.** The Responses format carries five tool shapes and only the plain function was forwarded; everything else was dropped silently. A real request arrives with 23 tool entries and 12 survive. Gone with them were the whole multi-agent surface (`namespace[collaboration]` — spawn_agent, wait_agent, send_message, followup_task, list_agents, interrupt_agent), `custom[apply_patch]`, and every MCP server configured (Grafana's 56 tools, pentest-ai's 50, context7, context-mode, node_repl). A dropped tool is indistinguishable from a tool the model was never given, which is why this presented for months as "routed models can't use MCP" rather than as an error. Namespaces are now unpacked into their inner functions and `custom` is forwarded.

- **The multi-agent toggle reported success while doing nothing.** It wrote `[features] multi_agent` — Codex's v1 collab surface, where the spawn tool is registered as `collaboration.spawn_agent` — while the orchestrator skill tells the model to call `spawn_agent`. No tool by that name existed, so the model reported having none and did the whole task itself. The toggle now writes `multi_agent_v2` (the flag that yields the plain name) and the merged catalog declares "v2" for routed models, matching what native entries already said.

- Follow-up to 0.2.4: the step that asks your shell where Codex lives now
  actually asks an interactive shell. zsh is the macOS default and only
  reads `.zshrc` for interactive shells — `.zprofile` and `.zlogin` cover
  login ones — so a login-only probe returned nothing on the setup it was
  meant to rescue, which is where most people put their `PATH`. Measured on
  a machine whose `PATH` lives in `.zshrc`: the login-only probe found
  nothing, the interactive one found the CLI. 0.2.4 still worked there
  because the known-locations fallback caught it; this makes the shell step
  carry its weight, which is what matters when Codex is installed somewhere
  unusual. The probe is bounded by a deadline so a slow shell profile cannot
  hang the screen that waits on it.

### Changed

- **The orchestrator skill stays in sync on every launch.** It was only
  rewritten when an agent was added or deleted, so an improvement to its
  text reached new installs and never existing ones — a user whose roster
  was already set up kept the version shipped the day they created their
  first agent. Syncing at startup makes the skill a product surface that can
  be corrected in a release rather than a file frozen at first write. A skill
  that cannot be written is a warning, never a refusal to start.

## 0.2.4

### Fixed

- **The Codex integration would not activate on a Mac where the app was
  opened normally.** Three of the four status rows stayed red — CLI
  detected, native catalog, merged catalog — and applying the integration
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
  to its full path — and the status row now says so instead of failing
  silently.

## 0.2.3

### Added

- **The menu bar is now a control surface.** Pick the active model, toggle a
  provider, and switch the whole routing on or off without opening the
  window. Turning it on starts the proxy and points Codex at it as one
  operation — and rolls the proxy back if pointing Codex fails, so a failed
  toggle never leaves you half-on.
- **The Agents screen is a catalogue, not a Codex feature list.** 22 agent
  roles that recur across the coding-agent ecosystem — reviewer, planner,
  debugger, adversarial critic, migration runner, incident responder, data
  analyst and more — grouped into eight categories. Picking one writes it
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
  silently dropped — which was most providers. Codex traffic was unaffected,
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
  damaged — removing the quarantine flag did not help, because the signature
  itself was invalid rather than merely untrusted. Builds are now ad-hoc
  signed. They are still not notarized, so the first launch shows the usual
  unidentified-developer prompt: right-click → Open once, or
  `xattr -dr com.apple.quarantine /Applications/LoomRouter.app`.
- **Credentials were written world-readable.** `~/.codex/config.toml` holds
  the local proxy token and was created at `0644`, which let any local
  process read it and spend the stored API keys through the proxy. Both it
  and `~/.loomrouter/config.json` are now owner-only.
- A first-run walkthrough: connect Codex, add a provider, set up agents.
