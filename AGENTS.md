# AGENTS.md - rules for code agents in this repo

Operational rules for any agent (Claude Code, opencode, etc.) working in this
repo. The goal is to keep work aligned with the conventions the repo already
follows and avoid traps that have already cost time.

## Documentation: do not add docs

- Do not create or commit new documentation files (`docs/*`, research, decision,
  roadmap, or plan `*.md` files). This includes `docs/*.md`.
- Design decisions and roadmaps live in code (`// why` comments) and git
  history, not in loose `.md` files.
- Exceptions: `README.md`, `CHANGELOG.md`, `CONTRIBUTING.md`, `SECURITY.md`,
  `CODE_OF_CONDUCT.md`, and the license are maintained, but only edit them when
  the request is explicitly about them.
- If you think "this change deserves docs", do not create a file. Leave a
  comment in the code explaining why, or talk to the user.

## Quality gate (always run before commit)

CI (`ci.yml`) runs these on every push/PR. Run them locally before committing:

```bash
bun run lint
bun run test
bun run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

- `rustfmt` differs between versions. CI uses `dtolnay/rust-toolchain@stable`.
  If `cargo fmt --check` fails locally, run `cargo fmt` and rebuild before
  blaming the formatter.
- `cargo fmt --check` must pass exactly as CI runs it
  (`--manifest-path src-tauri/Cargo.toml --check`).

## Multi-OS: Windows, macOS, and Linux

- Every change must work on all three OSes: Windows, macOS, and Linux. CI only
  builds/tests on `ubuntu-latest`, so passing the gate does not guarantee
  Windows/macOS. The person changing the code must verify those platforms too.
- When touching subprocess, shell, path, permission, or file secrecy code,
  verify against all three OSes before saying it is done:
  - CLI binary: `codex.exe` (Windows, `%LOCALAPPDATA%`) vs `codex` on PATH on
    macOS/Linux (`~/.local/bin`, Homebrew, etc.); `claude.cmd`/`claude.exe`/
    `claude`.
  - Config paths: never hardcode `/Users/...` or `%USERPROFILE%`; use
    `dirs::home_dir()`/`dirs::data_local_dir()`/`std::env::var("CODEX_HOME")`
    (see `codex.rs`, `claude_cli.rs`, `secure_fs.rs`).
  - Shell: macOS/Linux use `/bin/sh` + login shell; Windows does not, so guard
    with `#[cfg(unix)]` (pattern already used in `claude_cli.rs`).
  - File permissions (mode 0600, etc.) exist only on Unix, so guard with
    `#[cfg(unix)]`; Windows keeps the default (see `secure_fs.rs`).
- New code with `#[cfg(target_os)]`/`#[cfg(windows)]`/`#[cfg(unix)]` needs a
  `// why` comment explaining the OS difference and why it exists.
- If the change is unavoidably OS-specific, tell the user first. Do not assume
  only macOS matters.

## Git

- Direct pushes to `main` are blocked by repo policy. Work on a branch and open
  a PR (`gh pr create`).
- Use atomic commits in repo style (conventional):
  `feat(scope):`, `fix(scope):`, `test(scope):`, `chore(scope):`,
  `style(scope):`.
- Do not amend commits already pushed to the PR; create a new commit instead.
- Do not commit secrets, keys, or `.env` files.

## House style

- Comments explain the why, never the what.
- Frontend: React + Tailwind, types mirrored from `src-tauri/src`
  (see `src/types/index.ts`), UI strings via `useStrings()`/i18n (en/pt/es/zh),
  API mock in `src/lib/api.ts`.
- Backend: Rust, modules in `src-tauri/src`, Tauri commands in `lib.rs` ->
  `state.rs` -> `claude_cli.rs`/`codex.rs`/`proxy.rs`. Subprocess spawning must
  always happen in `spawn_blocking`.
- Tests: `cargo test` (Rust unit + `tests/e2e.rs`) and `vitest` (frontend).
