# Task 8 — translate

## Escopo e baseline

- Base verificada: `7ea32cd`.
- Baseline: `cargo test --manifest-path src-tauri/Cargo.toml translate` executou 52 testes, todos aprovados.
- O refactor apenas realocou código existente. Não foram criados helpers comportamentais; portanto não houve ciclo RED-GREEN novo. Os testes de caracterização existentes foram preservados, incluindo cobertura direta de `synthetic_id` e `flatten_tools`.

## Implementação

- `translate.rs` agora é uma facade de 25 linhas que declara os módulos e preserva os re-exports públicos.
- `tools.rs`: IDs sintéticos, namespaces, ferramentas e freeform.
- `request.rs`: conversões de Requests Responses para Chat/Anthropic.
- `response.rs`: conversões não-streaming de resposta, uso, namespace e freeform.
- `stream.rs`: `StreamTranslator`, eventos, IDs, reasoning, chamadas de ferramenta, usage, finalize/drain e `[DONE]`.
- `tests_a.rs` e `tests_b.rs` dividem os 52 testes sem enfraquecer as asserções.

## Verificação

- `cargo test --manifest-path src-tauri/Cargo.toml translate`: 52 passed, 0 failed.
- `cargo test --manifest-path src-tauri/Cargo.toml`: 254 unit + 9 e2e passed, 0 failed.
- `cargo fmt --manifest-path src-tauri/Cargo.toml --check`: passou.
- `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`: passou.
- `bun run lint`: passou.
- `bun run test`: 132 passed, 0 failed.
- `bun run build`: passou.
- `git diff --check`: passou.

## Tamanhos finais

| Arquivo | Linhas |
| --- | ---: |
| `translate.rs` | 25 |
| `translate/tools.rs` | 458 |
| `translate/request.rs` | 504 |
| `translate/response.rs` | 466 |
| `translate/stream.rs` | 887 |
| `translate/tests_a.rs` | 743 |
| `translate/tests_b.rs` | 539 |

Todos permanecem abaixo de 1000 linhas.

## Self-review e concerns

- Comparei a API pública com a base; não há símbolos públicos ausentes ou adicionais.
- A separação conserva as funções e os testes de streaming existentes, incluindo ordering, reasoning, usage, finalização e `[DONE]`.
- O linker do Windows ainda emite uma mensagem informativa ao gerar a import library; os testes e o clippy passam. O build Vite conserva o aviso pré-existente de chunk acima de 500 kB.
