<div align="center">

# LoomRouter

**Weave any model into your coding agent's picker.**

Use Kimi, DeepSeek, OpenRouter, Anthropic — any OpenAI-compatible endpoint —
**inside Codex's own model picker**, right next to the native GPT models.
With thinking summaries, vision, tool calls, and adjustable reasoning effort.

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Built with Rust + Tauri](https://img.shields.io/badge/Rust%20%2B%20Tauri-desktop-orange)](https://tauri.app)

<!-- TODO: add a screenshot of the Overview dashboard before publishing
![LoomRouter Overview](docs/images/overview.png) -->

</div>

---

## ✨ Features

- 🧵 **Models in the native picker** — external models show up in Codex's model
  list alongside GPT, with display name, context window and reasoning levels.
- 🔀 **Local proxy with full translation** — Responses API ⇄ Chat Completions
  ⇄ Anthropic Messages, including streaming, tool calls and reasoning.
- ⚡ **WebSocket transport (Codex v2)** — speaks the Responses-over-WebSocket
  protocol Codex now prefers, with per-connection conversation rebuild for
  providers without server-side turn storage. Plain HTTP/SSE still works.
- 🧠 **Thinking summaries** — provider reasoning streams (e.g. Kimi
  `reasoning_content`) are mapped to Codex's reasoning UI.
- 👁️ **Vision** — image inputs flow through to multimodal models like Kimi K3.
- 🎚️ **Reasoning effort in the picker** — Codex's low/medium/high/xhigh mapped
  to each provider's contract (e.g. Kimi's low/high/max).
- 📊 **Overview dashboard** — requests, input/output/cache tokens, cache-hit
  ratio, provider quotas (Kimi Code weekly + 5-hour window) and balances
  (OpenRouter, DeepSeek).
- 💾 **Cache-friendly** — byte-stable message prefixes so automatic context
  caching (Kimi: cached input at ~10% of the price) actually hits.
- 🔐 **Local-first credentials** — API keys never leave
  `~/.loomrouter/config.json`.
- 🤖 **Zero manual config** — apply the Codex integration once; provider and
  model changes are auto-applied from then on. Native GPT models keep working
  through the same proxy (ChatGPT login passthrough), including remote
  compaction.

## 🚀 Getting started

### Download

Grab the latest installer for your platform from
[Releases](../../releases) (Windows, macOS, Linux).

### From source

Prerequisites: [Bun](https://bun.sh) and a
[Rust toolchain](https://rustup.rs).

```bash
git clone https://github.com/<you>/loom-router.git
cd loom-router
bun install
bun run tauri dev
```

### Set up (about 1 minute)

1. **Add a provider** — pick a preset (Kimi Code, DeepSeek, OpenRouter…) or a
   custom endpoint, paste your API key, and hit **Fetch models**. The key is
   validated against the live model catalog.
2. **Toggle the models** you want in your agent's picker.
3. **Start the server** — the proxy listens on `127.0.0.1:4180`.
4. **Apply the Codex integration** — LoomRouter writes a clearly marked
   managed block into `~/.codex/config.toml` and a merged model catalog.
5. Restart Codex. Your external models are in the picker. 🎉

From then on, any provider or model change is applied automatically — you only
need to restart Codex to reload the catalog.

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
   block — your own settings are never touched.

## 🗺️ Roadmap

- [x] Provider management UI with live model discovery and key validation
- [x] Local proxy with full SSE translation across protocols
- [x] Codex merged-catalog integration (models in the native picker)
- [x] Responses-over-WebSocket transport (Codex v2)
- [x] Thinking summaries, vision, adjustable reasoning effort
- [x] Overview dashboard with quotas, balances and usage stats
- [ ] "Use without OpenAI login" mode (republish external models under native slugs)
- [ ] System tray with request activity
- [ ] Additional locales (i18n-ready; English is the source)

## 🛠️ Development

```bash
bun install
bun run tauri dev     # desktop app with hot reload
bun run dev           # frontend only, in the browser (mock backend)
cargo test --manifest-path src-tauri/Cargo.toml
```

```
src/            React UI (TypeScript, Tailwind, shadcn/ui)
src/i18n/       UI strings — English is the source locale
src-tauri/      Rust backend
  src/proxy.rs      local proxy, WebSocket transport, provider dispatch
  src/translate.rs  protocol translation (Responses / Chat / Anthropic)
  src/codex.rs      Codex config + merged catalog integration
  src/stats.rs      usage recording and aggregation
  src/config.rs     app config and credential storage
  src/providers.rs  built-in provider presets
```

## 🤝 Contributing

Issues and PRs are welcome. Please run `cargo test` and `bun run build` before
submitting.

## 📄 License

[MIT](LICENSE) © LoomRouter contributors
