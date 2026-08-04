# LoomRouter

**Weave any model into your coding agent's picker.**

LoomRouter is a small, local, credential-isolating router that makes external
models (DeepSeek, Kimi, OpenRouter, Groq, Anthropic, and any OpenAI-compatible
endpoint) appear **inside your coding agent's own model picker** — starting
with Codex, next to the native GPT models.

Built with **Rust + Tauri**. One binary, no Node/Python runtimes, no manual
config editing.

## Why

- Existing routers either require heavy manual setup (shell scripts, Python
  services, config file editing) or force you to switch models in their own UI
  instead of your agent's picker.
- LoomRouter does the integration work for you from a simple desktop UI:
  add a provider, fetch its live model catalog, toggle the models you want,
  start the server, apply the Codex integration. Done.

## How it works

1. **Local proxy** (`127.0.0.1:4180`) receives requests from your agent and
   dispatches them to the right provider based on the `model` field,
   translating between the Responses API, Chat Completions, and Anthropic
   Messages formats.
2. **Catalog merger** writes `merged-models.json`: the native Codex catalog
   plus every external model you enabled.
3. **Managed config block** in `~/.codex/config.toml` (clearly marked
   `# BEGIN/END loom-router-managed`) points Codex at the proxy and the merged
   catalog. Removing the integration deletes exactly that block — your own
   settings are never touched.

API keys are stored only in `~/.loomrouter/config.json` on your machine.

## Development

Prerequisites: [Bun](https://bun.sh) and a Rust toolchain.

```bash
bun install
bun run tauri dev     # desktop app with hot reload
bun run dev           # frontend only, in the browser (mock backend)
cargo test --manifest-path src-tauri/Cargo.toml
```

## Project layout

```
src/            React UI (TypeScript, Tailwind, shadcn/ui)
src/i18n/       UI strings — English is the source locale
src-tauri/      Rust backend
  src/proxy.rs      local proxy + provider dispatch
  src/translate.rs  protocol translation (Responses / Chat / Anthropic)
  src/codex.rs      Codex config + merged catalog integration
  src/config.rs     app config and credential storage
  src/providers.rs  built-in provider presets
```

## Roadmap

- [x] Provider management UI with live model discovery
- [x] Local proxy with provider dispatch
- [x] Codex merged-catalog integration (models in the native picker)
- [ ] Full SSE translation (Responses API events ↔ chat.completion.chunk)
- [ ] Tool-call shape preservation across protocols
- [ ] "Use without OpenAI login" mode (republish external models under native slugs)
- [ ] System tray with request activity
- [ ] Additional locales (i18n-ready; English is the source)

## License

MIT
