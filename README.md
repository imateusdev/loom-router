<div align="center">

# LoomRouter

**Weave any model into your coding agent's picker.**

Use Kimi, DeepSeek, OpenRouter, Anthropic - any OpenAI-compatible endpoint -
**inside Codex's own model picker**, right next to the native GPT models.
With thinking summaries, vision, tool calls, and adjustable reasoning effort.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Built with Rust + Tauri](https://img.shields.io/badge/Rust%20%2B%20Tauri-desktop-orange)](https://tauri.app)

<img src="docs/images/codex-picker.png" width="820"
     alt="Codex's model picker, open. The native models 5.6 Terra, 5.6 Luna, 5.5 and 5.4 Mini are listed first, and directly below them, in the same list, sit kimi-for-coding, kimi-for-coding-highspeed, k3, k3-256k and ~deepseek/deepseek-v4-flash-latest - all routed through LoomRouter.">

<sub>Codex's own picker. Native GPT models on top; everything below them is
routed through LoomRouter - same list, same shortcuts, no separate mode.</sub>

</div>

---

## ✨ Features

- 🧵 **Models in the native picker** - external models show up in Codex's model
  list alongside GPT, with display name, context window and reasoning levels.
- 🔀 **Local proxy with full translation** - Responses API ⇄ Chat Completions
  ⇄ Anthropic Messages, including streaming, tool calls and reasoning.
- ⚡ **WebSocket transport (Codex v2)** - speaks the Responses-over-WebSocket
  protocol Codex now prefers, with per-connection conversation rebuild for
  providers without server-side turn storage. Plain HTTP/SSE still works.
- 🧠 **Thinking summaries** - provider reasoning streams (e.g. Kimi
  `reasoning_content`) are mapped to Codex's reasoning UI.
- 👁️ **Vision** - image inputs flow through to multimodal models like Kimi K3.
- 🎚️ **Reasoning effort in the picker** - Codex's low/medium/high/xhigh mapped
  to each provider's contract (e.g. Kimi's low/high/max).
- 📊 **Overview dashboard** - requests, input/output/cache tokens, cache-hit
  ratio, provider quotas (Kimi Code weekly + 5-hour window) and balances
  (OpenRouter, DeepSeek).
- 💾 **Cache-friendly** - byte-stable message prefixes so automatic context
  caching (Kimi: cached input at ~10% of the price) actually hits.
- 🔐 **Local-first credentials** - API keys never leave
  `~/.loomrouter/config.json`.
- 🤖 **Zero manual config** - apply the Codex integration once; provider and
  model changes are auto-applied from then on. Native GPT models keep working
  through the same proxy (ChatGPT login passthrough), including remote
  compaction.
- 🌀 **OpenCode Zen/Go ready** - built-in presets for Zen's three API
  dialects (Chat Completions, Anthropic Messages and a native Responses
  passthrough for its GPT/Grok models), so a Zen key puts Kimi K3, GLM,
  DeepSeek, MiniMax, Claude, Qwen, GPT and Grok into your agent's picker.

## 🚀 Getting started

### Download

Grab the latest installer for your platform from
[Releases](../../releases) (Windows, macOS, Linux).

### From source

Prerequisites: [Bun](https://bun.sh) and a
[Rust toolchain](https://rustup.rs).

```bash
git clone https://github.com/imateusdev/loom-router.git
cd loom-router
bun install
bun run tauri dev
```

### Set up (about 1 minute)

1. **Add a provider** - pick a preset (Kimi Code, DeepSeek, OpenRouter…) or a
   custom endpoint, paste your API key, and hit **Fetch models**. The key is
   validated against the live model catalog.
2. **Toggle the models** you want in your agent's picker.
3. **Start the server** - the proxy listens on `127.0.0.1:4180`.
4. **Apply the Codex integration** - LoomRouter writes a clearly marked
   managed block into `~/.codex/config.toml` and a merged model catalog.
5. Restart Codex. Your external models are in the picker. 🎉

<div align="center">

<img src="docs/images/providers.png" width="820"
     alt="LoomRouter's Providers page. Two provider cards side by side: Kimi Code - Coding Plan with four models enabled (k3 at 1M context, k3-256k, kimi-for-coding and kimi-for-coding-highspeed at 256K), and OpenRouter with a searchable model list where deepseek-v4-flash-latest is toggled on.">

<sub>The Providers page: steps 1 and 2. Each model has its own toggle, so the
picker only shows what you actually use.</sub>

</div>

From then on, any provider or model change is applied automatically - you only
need to restart Codex to reload the catalog.

### Visual assistance for text-only models

Visual assistance is a **global opt-in** in the Codex settings. Enable it only
when you want a text-only routed model to receive image-derived evidence. Pick
one vision-capable **primary visual assistant**, then add an explicit,
ordered list of vision-capable fallback models. The primary is tried first;
fallbacks are only tried in the order shown for provider timeouts, rate/quota
responses (HTTP 429), and server failures (HTTP 5xx). Other failures stop the
chain so an invalid configuration or rejected request is not retried elsewhere.

Remote image retrieval has a 10-second limit, while the proxy's provider
request limit is 10 minutes. If every configured visual assistant is exhausted,
LoomRouter returns a gateway error and **does not silently send the raw image
to a text-only model**. Fix the provider/quota issue or adjust the explicit
assistant list, then retry.

For privacy and speed, LoomRouter keeps a small in-memory cache (up to 64
entries for 5 minutes) of structured visual evidence and the model that
produced it. Source image bytes are used only to derive the cache key and are
not retained by the cache. The Logs page records only a successful visual
analysis label for each image, with its model, attempt count, duration, and
cache hit/miss;
it never records API keys, image bytes, raw prompts, or visual evidence.

## 🖥️ Overview dashboard

The home screen shows, for today / 24h / 7d / 30d:

- per-provider cards with **quota bars** (Kimi Code weekly allowance and
  rolling 5-hour window) and **balances** (OpenRouter credits, DeepSeek);
- **requests, input/output/cache tokens and cache-hit ratio**;
- per-provider usage breakdown.

## 🔌 Other agents: opencode

LoomRouter speaks both `/v1/chat/completions` and `/v1/responses`, so any
agent with an OpenAI-compatible provider option works. For
[opencode](https://opencode.ai), add a custom provider to `opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "loomrouter": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "LoomRouter (local)",
      "options": { "baseURL": "http://127.0.0.1:4180/v1", "apiKey": "loom" },
      "models": {
        "kimi-coding/k3": { "name": "Kimi K3 (via LoomRouter)" }
      }
    }
  }
}
```

Model IDs are the same slugs shown in the LoomRouter Providers page
(`GET http://127.0.0.1:4180/v1/models` lists them). LoomRouter does not
require an API key; opencode just wants a non-empty value.

## 🔧 How it works

1. **Local proxy** (`127.0.0.1:4180`) receives requests from your agent over
   HTTP/SSE or WebSocket and dispatches them by the `model` field, translating
   between the Responses API, Chat Completions and Anthropic Messages formats.
   Requests for native GPT models are forwarded to OpenAI's backend with your
   own ChatGPT headers untouched.
2. **Catalog merger** writes `~/.codex/loom-router/merged-models.json`: the
   native Codex catalog plus one entry per enabled external model (context
   window, vision, reasoning levels, model-neutral identity).
3. **Managed config block** in `~/.codex/config.toml`
   (`# BEGIN/END loom-router-managed`) defines a `loomrouter` provider with
   `wire_api = "responses"` and `supports_websockets = true`, and points Codex
   at the merged catalog. Removing the integration deletes exactly that
   block - your own settings are never touched. Before each write the current
   file is backed up to `config.toml.bak` and the new content is installed
   via a temp-file + rename, so an interrupted write can never truncate your
   config.

## 🔐 Security & environment variables

- **API keys** live only in `~/.loomrouter/config.json` (directory `0700`,
  file `0600` on Unix; on Windows, permission tightening is best-effort and
  the file relies on your profile directory's ACLs). Keys are never sent to
  the app's webview - the UI only sees whether a key exists.
- **Local proxy token** - the proxy on `127.0.0.1:4180` requires a random
  bearer token generated at each startup. LoomRouter injects it into the
  managed block of `~/.codex/config.toml` (`http_headers`) so Codex can
  authenticate; other agents must send it too.

The following environment variables are **escape hatches for development
and debugging**. They are powerful and dangerous - only set them if you
understand exactly why you need them:

| Variable | Effect | Risk |
| --- | --- | --- |
| `CODEX_BIN` | Path to the Codex CLI binary LoomRouter runs for catalog capture (`codex debug models`). | **Arbitrary code execution**: pointing this at an untrusted path makes LoomRouter execute that binary. |
| `CODEX_NATIVE_BASE_URL` | Overrides the upstream base URL used for native ChatGPT/OpenAI passthrough requests. | **Credential exfiltration**: your ChatGPT token is forwarded to whatever host this points to. Never set it to a host you don't fully control. |
| `CODEX_HOME` | Overrides the Codex config directory (default `~/.codex`). | LoomRouter will read and modify the `config.toml` inside that directory. |

## 🗺️ Roadmap

- [x] Provider management UI with live model discovery and key validation
- [x] Local proxy with full SSE translation across protocols
- [x] Codex merged-catalog integration (models in the native picker)
- [x] Responses-over-WebSocket transport (Codex v2)
- [x] Thinking summaries, vision, adjustable reasoning effort
- [x] Overview dashboard with quotas, balances and usage stats
- [x] Agents page: manage Codex subagents (`~/.codex/agents/`) from the UI -
  pick a routed model, reasoning effort and instructions per agent, so a
  session on one provider can delegate to workers on another (e.g. Kimi
  orchestrating DeepSeek workers)
- [x] Background/auxiliary call routing: optional fallback model for Codex's
  side calls (compaction, prewarm, memory - detected via
  `x-codex-turn-metadata`) so they can run on a cheap/free provider instead
  of the main turn's destination
- [x] "Use without OpenAI login" mode (managed block with
  `requires_openai_auth = false`; external models republished under bare
  slugs, native GPT models hidden)
- [x] System tray with request activity (requests/hour, last request,
  live-updating menu and tooltip)
- [x] Additional locales (i18n-ready; English is the source; Português (BR),
  简体中文 and Español included)

## 🛠️ Development

```bash
bun install
bun run tauri dev     # desktop app with hot reload
bun run dev           # frontend only, in the browser (mock backend)
cargo test --manifest-path src-tauri/Cargo.toml
```

```
src/            React UI (TypeScript, Tailwind, shadcn/ui)
src/i18n/       UI strings - English is the source locale
src-tauri/      Rust backend
  src/proxy.rs      local proxy, WebSocket transport, provider dispatch
  src/translate.rs  protocol translation (Responses / Chat / Anthropic)
  src/codex.rs      Codex config + merged catalog integration
  src/stats.rs      usage recording and aggregation
  src/config.rs     app config and credential storage
  src/providers.rs  built-in provider presets
```

## 📦 Building installers

```bash
bun install
bun run tauri build
```

Artifacts land in `src-tauri/target/release/bundle/`:

- **Windows** (must build on Windows): NSIS setup `.exe` and `.msi`
- **macOS** (must build on macOS - no cross-compile): `.dmg` and `.app`.
  The bundle is ad-hoc signed (`bundle.macOS.signingIdentity: "-"`), not
  notarized. Without that setting Tauri does not sign the bundle at all: the
  binary keeps only its linker signature, `Contents/_CodeSignature/` is never
  written, and macOS rejects the app outright - `codesign --verify` reports
  "code has no resources but signature indicates they must be present" and
  the app opens as "damaged", which removing the quarantine does **not**
  fix. That was the state of the v0.2.0 downloads.

  Ad-hoc signed but unnotarized, first launch instead hits the ordinary
  unidentified-developer prompt. Right-click → Open once, or:
  `xattr -dr com.apple.quarantine /Applications/LoomRouter.app`
  For a frictionless install, sign with an Apple Developer certificate and
  notarize; otherwise every user needs one of the two steps above.

For releases without owning a Mac, this repo includes
`.github/workflows/release.yml`: push a `v*` tag and it runs the full
quality gate (ESLint, frontend build, `cargo fmt`/`clippy`/`test`), then
builds Windows (NSIS + MSI) and macOS (Apple Silicon + Intel) installers
with `tauri-apps/tauri-action` and publishes them to a GitHub Release with
generated notes. The tag version must match `src-tauri/tauri.conf.json`,
`package.json` and `src-tauri/Cargo.toml` (the workflow fails early
otherwise). macOS builds are ad-hoc signed but not notarized: on first
launch, right-click → Open once, or run
`xattr -dr com.apple.quarantine /Applications/LoomRouter.app` (see
"Building installers" above).

**Auto-update:** installed apps check `releases/latest/download/latest.json`
on startup and offer to download, install and relaunch (via
`tauri-plugin-updater`). Update artifacts are signed with a minisign key:
the private key must be set as the `TAURI_SIGNING_PRIVATE_KEY` repo secret
(plus `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` if the key has one) so the
release workflow can sign `latest.json`. Keep the private key safe - losing
it means shipping a new manual installer.

## 🤝 Contributing

Issues and PRs are welcome. Please run `cargo test` and `bun run build` before
submitting.

## 📄 License

[MIT](LICENSE) © LoomRouter contributors
