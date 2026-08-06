# Contributing to LoomRouter

This file writes down the conventions the codebase already follows, and adds
the rules that came out of auditing it. Nothing here is aspirational — every
rule below exists because breaking it has already cost this project a bug.

## The quality gate

`.github/workflows/release.yml` runs these on every version tag, and a
failure blocks the release before a single installer is built. Run them
locally before pushing:

```bash
bun run lint
bun run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

**The gate only runs on tags.** That is the single biggest hole in this
project's process: between `v0.2.0` and the commit that followed it, `main`
accumulated a frontend type error that broke `bun run build` and a
formatting drift that broke `cargo fmt --check`. Neither was noticed, because
nothing ran. Until the workflow also runs on push and pull request, run the
five commands above yourself before merging anything.

Rust toolchain: CI uses `dtolnay/rust-toolchain@stable`. `rustfmt` output
differs between releases, so a local toolchain older than CI's will produce
formatting the gate rejects. Check `rustc --version` against the version in
the last CI log before blaming the formatter.

## House style

The code reads a certain way, and staying consistent matters more than any
individual preference here.

- **Comments explain _why_, never _what_.** The codebase is full of comments
  like "Kimi rejects the Responses-era `developer` role; it is semantically
  the system prompt, so downgrade it." That is the standard. A comment
  restating the line below it is noise; a comment recording the provider
  quirk that forced the line is the most valuable thing in the file.
- **Record the bug in the fix.** When a change exists because something was
  broken, say so at the fix site. Future readers need to know the shape of
  the failure to avoid reintroducing it.
- **Tests live beside the code** in `#[cfg(test)] mod tests` at the bottom of
  the module, with cross-module behaviour in `src-tauri/tests/e2e.rs`.
- **Name tests as the sentence they assert**:
  `normalize_usage_reads_kimi_per_choice_placement`, not `test_usage_2`.
- **Failures are logged, never swallowed.** `let _ = something_fallible()`
  hides exactly the class of bug that is hardest to find.

## DRY rules

These come from concrete duplication found in this repo. Each one names the
bug it prevents.

### 1. One owner per piece of protocol knowledge

Where a provider puts its data, and what it calls it, is knowledge that
belongs to `translate.rs` and nowhere else.

Usage counts were read directly in five places using Responses field names.
Any upstream that answered in another dialect reported zero tokens and was
silently dropped, so the dashboard stayed empty for every OpenAI-compatible
client. The field names now live once, in `translate::normalize_usage`.

> **Rule:** never reach into a provider payload with a literal field name
> outside `translate.rs`. Add a case to the normalizer instead.

### 2. One implementation of a security-relevant operation

Writing a file atomically was implemented three times. Two of them tightened
permissions; the third did not — and it was the one writing the local proxy
token into `~/.codex/config.toml`, which landed at `0644`. Anything that can
read that token can spend the stored API keys. All three now share
`secure_fs`.

> **Rule:** if an operation has a security property, there is exactly one
> function with that property and everything calls it. A second copy is a
> second chance to forget the hardening.

### 3. Per-OS differences live in one module

Unix modes versus Windows ACLs is a real difference, but it should appear in
one place. Scattering `#[cfg(unix)]` across call sites is how one branch ends
up hardened and another does not.

> **Rule:** put the `cfg` inside the shared helper, never at the call site.
> Where a platform genuinely cannot offer the guarantee — Windows ACLs today
> — document it on the function so callers cannot mistake success for safety.

### 4. Don't rebuild a screen that already exists

The first-run walkthrough hands off to the real Providers page rather than
reimplementing its form, so a provider is added and validated in one place.

> **Rule:** onboarding, wizards, and empty states link to the canonical
> screen. Duplicating a form duplicates its validation, and the copy drifts.

### 5. The browser mock mirrors the backend contract

`src/lib/api.ts` falls back to an in-memory mock outside Tauri. A mock that
returns frozen literals silently diverges: `codex_status` was missing three
fields of `CodexStatus` (hidden by an `as T` cast) and never reflected
`codex_apply`, so no success state could be previewed.

> **Rule:** when a command changes backend state, the mock changes mock
> state. Keep mock responses complete against their TypeScript type.

### 6. One source of truth for the version

The version currently lives in `package.json`, `src-tauri/tauri.conf.json`
and `src-tauri/Cargo.toml`, and CI only checks the first two — which is why
`Cargo.toml` sat at `0.1.0` while the app shipped `0.2.0`.

> **Rule:** bump all three together, and keep them in the tag-verification
> step so drift fails the release instead of shipping.

## Credentials

- API keys stay in `~/.loomrouter/config.json`; the token stays in the
  managed block of `~/.codex/config.toml`. Both are written through
  `secure_fs` and are owner-only on Unix.
- `get_config` blanks keys before they reach the webview — the UI only ever
  learns `has_key`. Keep it that way.
- **Never log a request or response body.** They carry user prompts and
  source code. Log metadata: path, status, byte length.
