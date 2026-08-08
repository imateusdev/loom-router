# AGENTS.md — regras para agentes de código neste repo

Regras operacionais para qualquer agente (Claude Code, opencode, etc.)
trabalhando neste repo. O objetivo: manter o trabalho dentro das convenções
que o repo já segue e evitar as armadilhas que já custaram tempo.

## Documentação: NÃO subir docs

- **Não criar nem commitar arquivos de documentação** novos (`docs/*`, `*.md`
  de pesquisa, decisão, roadmap ou plano). Isso inclui `docs/*.md`.
- Decisões de design e roadmaps vivem no código (comentários `// why`) e no
  histórico do git, não em arquivos `.md` soltos.
- Exceções: `README.md`, `CHANGELOG.md`, `CONTRIBUTING.md`, `SECURITY.md`,
  `CODE_OF_CONDUCT.md` e licença são mantidos — mas só editá-los quando o
  pedido for explicitamente sobre eles.
- Se achar que "essa mudança merece uma doc", **não crie o arquivo**. Deixe um
  comentário no código explicando o porquê, ou fale com o usuário.

## Quality gate (rodar sempre antes de commit)

CI (`ci.yml`) roda estes em todo push/PR — rode localmente antes de commitar:

```bash
bun run lint
bun run test
bun run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
```

- `rustfmt` difere entre versões. CI usa `dtolnay/rust-toolchain@stable`.
  Se `cargo fmt --check` falhar localmente, rode `cargo fmt` e recompile
  antes de acusar o formatter.
- `cargo fmt --check` tem que passar **exatamente** como o CI roda
  (`--manifest-path src-tauri/Cargo.toml --check`).

## Multi-OS: Windows, macOS e Linux

- **Toda mudança tem que funcionar nos três OS** (Windows, macOS, Linux).
  O `ci.yml` compila/testa só em `ubuntu-latest`, então a correção do gate não
  garante Windows/macOS — quem muda o código tem que checar por conta própria.
- Ao tocar qualquer código de subprocesso, shell, path, permissão ou sigilo de
  arquivo, verificar contra os três OS antes de dizer que está pronto:
  - Binário do CLI: `codex.exe` (Windows, `%LOCALAPPDATA%`) vs `codex` no PATH
    macOS/Linux (`~/.local/bin`, homebrew, etc.); `claude.cmd`/`claude.exe`/`claude`.
  - Paths de config: nunca hardcode `/Users/...` nem `%USERPROFILE%` — usar
    `dirs::home_dir()`/`dirs::data_local_dir()`/`std::env::var("CODEX_HOME")`
    (ver `codex.rs`, `claude_cli.rs`, `secure_fs.rs`).
  - Shell: macOS/Linux usam `/bin/sh` + login shell; Windows não tem isso —
    proteger com `#[cfg(unix)]` (padrão já usado em `claude_cli.rs`).
  - Permissões de arquivo (modo 0600 etc.): só existem no Unix —
    proteger com `#[cfg(unix)]`, Windows deixa default (ver `secure_fs.rs`).
- Código novo com `#[cfg(target_os)]`/`#[cfg(windows)]`/`#[cfg(unix)]` precisa
  de um comentário `// why` explicando a diferença de OS e por que ela existe.
- Se a mudança for inevitavelmente específica de um OS, falar com o usuário
  antes — não assumir que só macOS importa.

## Git

- Push direto para `main` é bloqueado por regra do repo. Trabalhe em branch e
  abra PR (`gh pr create`).
- Commits atômicos no estilo do repo (conventional):
  `feat(scope):`, `fix(scope):`, `test(scope):`, `chore(scope):`, `style(scope):`.
- Não amende commits que já foram para o PR; faça um commit novo.
- Não commite secrets, keys ou `.env`.

## House style

- Comentários explicam o **porquê**, nunca o **o quê**.
- Frontend: React + Tailwind, tipos espelhados de `src-tauri/src` (ver
  `src/types/index.ts`), strings de UI via `useStrings()`/i18n (en/pt/es/zh),
  mock de API em `src/lib/api.ts`.
- Backend: Rust, módulos em `src-tauri/src`, comandos Tauri em `lib.rs` →
  `state.rs` → `claude_cli.rs`/`codex.rs`/`proxy.rs`. Spawn de subprocesso
  sempre em `spawn_blocking`.
- Testes: `cargo test` (Rust unit + `tests/e2e.rs`) e `vitest` (frontend).
