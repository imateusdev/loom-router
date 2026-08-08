# claude-code provider: research, decisions and roadmap

Status atual da integração do claude-code no LoomRouter, o que foi pesquisado,
o que já foi ajustado e o que ainda está pendente (Fase 2 e failover multi-conta).

---

## Objetivo

Usar a assinatura Claude (plano Max) dentro do picker do Codex (LoomRouter):
catálogo completo (modelos, context window, fast mode) e autenticação via spawn
do CLI oficial `claude` — **sem extração de token OAuth** (banido pela Anthropic
em jan/2026). O LoomRouter nunca vê nem guarda o credential; apenas sonda se o
CLI está presente/logado e (na Fase 2) spawna o binário real.

---

## O que já está feito (Fase 1 — catálogo/auth/UI)

- Provider `claude-code` no catálogo, protocolo `Anthropic`, **sem api_key** —
  a credencial é o login que o usuário já fez dentro do Claude Code CLI/Desktop.
- Catálogo curado (`providers.rs::CLAUDE_CODE_MODELS`), 5 modelos:

  | Modelo            | Context | fast mode |
  |-------------------|---------|-----------|
  | claude-fable-5    | 1M      | não       |
  | claude-opus-5     | 1M      | sim       |
  | claude-opus-4-8   | 1M      | sim       |
  | claude-sonnet-4-6 | 1M      | não       |
  | claude-haiku-4-5  | 200K    | não       |

- `claude_cli.rs`:
  - `claude auth status` parseado em `ClaudeAuthStatus` (logged_in, auth_method,
    subscription_type, email, plan). Roda em `spawn_blocking`.
  - Resolução do binário `claude` **multi-plataforma**, cacheada em `OnceLock`
    (ver "Bug: CLI não encontrado no PATH" abaixo).
  - Nenhuma chamada direta à API Anthropic; sem token lido do disco.
- UI (aba Providers): dialog de add/edit esconde key/baseUrl pro claude-code,
  badge "Max"/fast mode, `dialectsInUse()`, i18n em 4 idiomas (en/pt/es/zh),
  key `claudeNoKey`. Tipos/mocks em `api.ts` e `types/index.ts`.
- Estados no backend: `state.rs` lida com claude-code em `list_models_detailed`,
  `save_provider` (stampa context_window/fast_mode e re-seeda o catálogo) e
  `fetch_balance` (reporta plan/email em vez de quota remota).

---

## Bugs pesquisados, causas e o que foi feito

### Bug: Codex ficava em "reconnecting" e não retornava resultado

Investigado a fundo; **causa raiz dupla**, ambas confirmadas por reprodução:

1. **Token do proxy é por processo.** `local_token()` (proxy.rs:59) é um
   `OnceLock` regenerado a cada restart do app; `codex.rs::apply` reescreve
   `~/.codex/config.toml` com o token novo. Se o Codex Desktop já estava aberto,
   ele mantém o token antigo → WS upgrade rejeitado (401/non-101) → reconnecting
   eterno. Confirmado: token antigo recusado após restart.
   → **Fix proposto**: persistir o token entre restarts (aguardando decisão).
2. **Slug morto por colapso do merge.** A sessão do usuário (iniciada pré-merge)
   usava `opencode-go-chat/deepseek-v4-flash`; o merge renomeou para
   `opencode-go`. Slug antigo → `resolve()` falha → cai em `EffectiveRoute::Native`
   → chatgpt.com rejeita com `400: model is not supported when using Codex with
   a ChatGPT account`. Confirmado no rollout: `~/.codex/sessions/...`.
   → **Fix proposto**: aliases de compatibilidade `opencode-go-chat/-claude/-responses`
   → `opencode-go` (aguardando decisão).

**Workaround atual**: reiniciar o Codex Desktop e re-selecionar o modelo após
cada restart do app.

### Bug: "claude CLI não encontrado no PATH"

**Causa**: `claude_binary()`/`find_in_path` só checavam o `PATH` do processo.
App aberto pelo Finder herda o PATH do launchd (`/usr/bin:/bin:/usr/sbin:/sbin`),
que não contém o bin de package manager → nunca achava `~/.local/bin/claude`.

**Fix aplicado** (`claude_cli.rs`, espelhando `codex.rs::resolve_codex_bin`,
cacheado em `OnceLock`):
1. `CLAUDE_BIN` explícito (escape hatch, igual ao `CODEX_BIN`);
2. PATH do processo (`claude.cmd`/`claude.exe`/`claude` no Windows);
3. login shell `$SHELL -lic "command -v claude"` com deadline de 3s — `-lic` (e
   não `-lc`) porque o zsh só carrega `.zshrc` em shell *interativo*; a última
   linha não-vazia é verificada com `--version` (evita banner/prompt);
4. locais conhecidos por plataforma: `~/.local/bin`, `~/.claude/local`,
   `~/.bun/bin`, `~/.volta/bin`, `~/.npm-global/bin`, `~/.yarn/bin`,
   `/opt/homebrew/bin`, `/usr/local/bin`, `/opt/local/bin` + fallback pro mais
   novo em `~/.local/share/claude/versions/*/claude`; Windows:
   `%USERPROFILE%\.local\bin`, `%APPDATA%\npm`.

Cada candidato é validado com `claude --version` (guarda contra shim quebrado).
Teste de regressão `resolves_the_cli_under_the_launchd_path` reproduz o cenário
Finder (muta a PATH pra `/usr/bin:/bin:/usr/sbin:/sbin`) — espelho do teste do
codex em `codex.rs`. Mensagem de erro atualizada, mantendo "not found" pra UI
continuar mapeando pra key `claudeCliMissing`.

---

## Pendente (Fase 2 — bridge `claude -p`)

O guard `dispatch_routed` (proxy.rs:~764) ainda retorna erro explícito
"claude-code backend is not wired yet..." para qualquer request claude-code.

**Decisão pendente — semântica de tools:**
1. **Autônomo** (recomendado pra desbloquear rápido): Claude age com
   `--allowedTools`, entrega só a resposta final; o Codex não vê tool calls
   intermediárias.
2. **Ponte fiel ao protocolo**: `--disallowedTools '*'` + sessões `--resume`,
   eventos `tool_use` devolvidos ao Codex. Mais complexo.

Quando implementar o spawn, a credencial vem do login do CLI (nunca token). Se
houver multi-conta (ver abaixo), spawnar com `CLAUDE_CONFIG_DIR` por conta.

---

## Roadmap: failover multi-conta (decidido, não implementado)

Pesquisa concluída, decisões de design fechadas com o usuário.

### Escopo
- **claude-code**: lista de contas por `CLAUDE_CONFIG_DIR` (cada uma com login
  próprio); se banir, cair pra login único.
- **key-based** (kimi, opencode-go, openrouter): lista de keys.

### Fluxo de failover (semântica fechada)
1. Request chega → proxy resolve a **conta ativa**: primeira não-esgotada,
   `is_main` primeiro, depois as auxiliares em ordem.
2. Resposta upstream 429/"limits exceeded"/"quota" → marca a conta
   `exhausted_at = now`, persiste, emite evento Tauri `account-exhausted` pra UI.
3. **O request atual falha** — o próprio Codex interpreta o erro e mostra na
   tela (sem retry transparente do mesmo request; streaming quebraria).
4. **O próximo request já cai na próxima conta** automaticamente.
5. **Todas esgotadas** → volta pra conta A, mas se o ciclo já rodou inteiro e
   segue abaixo do reset, retorna erro claro de "cota esgotada em todas as contas".

### Reset tracking
- Inferir automaticamente quando possível: headers de rate-limit
  (`x-ratelimit-reset`, `retry-after`) capturados no momento do estouro + tipo
  de plano (Kimi mensal, Anthropic rolling).
- Override manual por conta (editar dia de reset na UI).

### Modelo de dados (`config.rs`)
- Novo `ProviderAccount`: `{ id, name, api_key?, config_dir?, is_main, enabled,
  exhausted_at?, next_reset? }`.
- `Provider.accounts: Vec<ProviderAccount>` — **migração**: `api_key` único vira
  `accounts[0]` com `is_main` na carga.

### Camadas afetadas
- `proxy.rs`: resolução da conta ativa + detecção de estouro + evento.
- `apply_provider_auth`: usar a key da conta ativa (hoje usa `p.api_key`).
- `claude_cli.rs`: spawn com `CLAUDE_CONFIG_DIR` da conta (Fase 2).
- `state.rs`: `save_provider` aceitar lista de contas; `fetch_balance` na main.
- UI `Providers.tsx` + `api.ts` + `i18n/*`: editor de contas, aviso de estouro,
  override de reset.

### Ordem sugerida de implementação
1. Fundação: modelo `ProviderAccount` + migração + resolução de conta ativa no proxy.
2. Detecção de estouro + evento `account-exhausted` + UI de aviso.
3. Editor de contas na UI + reset automático/override.
4. Fase 2 (`claude -p`) já sobre o modelo multi-conta.

---

## Pendências abertas (decisões do usuário)

- [ ] Persistir o token do proxy entre restarts (fix do "reconnecting").
- [ ] Aliases de slug `opencode-go-chat/-claude/-responses` → `opencode-go`.
- [ ] Semântica de tools da Fase 2 (autônomo vs ponte fiel).
- [ ] "Se banir" multi-conta claude-code → cair pra login único.
