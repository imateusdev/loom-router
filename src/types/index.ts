// LoomRouter shared types (mirror of src-tauri Rust structs).

export type ProviderProtocol = 'openai' | 'anthropic' | 'responses'

export interface ProviderModel {
  id: string
  label?: string | null
  enabled: boolean
}

export interface Provider {
  id: string
  name: string
  protocol: ProviderProtocol
  base_url: string
  // S4 contract: the backend never returns the real key (always "" on read).
  // On save, "" means "keep the existing key".
  api_key?: string | null
  // True when the backend already holds a key for this provider.
  has_key: boolean
  user_agent?: string | null
  models: ProviderModel[]
  enabled: boolean
}

/// Context window published to Codex for one model.
export interface ContextWindow {
  /// Tokens.
  window: number
  /// False when this is only the conservative fallback — nothing is actually
  /// known about the model's limit.
  known: boolean
}

export interface AppConfig {
  port: number
  providers: Record<string, Provider>
  // Model slug used for Codex side/background calls (title generation,
  // compaction, etc.); null keeps the Codex default.
  side_call_fallback: string | null
  // When true, external models are exposed as native slugs so Codex can be
  // used without an OpenAI login.
  native_slug_mode: boolean
  // Model Codex starts new sessions with, as "provider/model"; null leaves
  // the choice to Codex. Written to the root `model` key of its config.toml.
  active_model?: string | null
  // Whether the first-run walkthrough has been finished. Absent (undefined)
  // only on a genuinely fresh install — the backend backfills `true` for any
  // config that predates the walkthrough, so upgrades never replay it.
  onboarding_completed?: boolean | null
}

// A Codex agent profile (~/.codex/agents). Mirrors the Rust AgentInfo struct.
export interface AgentInfo {
  name: string
  // What Codex reads to decide when this agent fits a task; empty on save
  // keeps/derives it.
  description: string
  model: string | null
  effort: string | null
  // "read-only" | "workspace-write" | null (inherit session policy).
  sandbox_mode: string | null
  instructions: string
}

// A ready-made agent recipe from the built-in gallery (Rust AgentTemplate).
export interface AgentTemplate {
  id: string
  label: string
  blurb: string
  description: string
  instructions: string
  sandbox_mode: string | null
}

export interface ServerStatus {
  running: boolean
  port: number
  url?: string | null
}

export interface CodexStatus {
  codex_home: string
  config_exists: boolean
  managed_block_present: boolean
  native_catalog_present: boolean
  merged_catalog_present: boolean
  merged_model_count: number
  codex_cli_available: boolean
  integration_enabled: boolean
}

/// One model's behaviour over the summarised window — the numbers that
/// actually distinguish models from each other.
export interface ModelAggregate {
  model: string
  requests: number
  errors: number
  input_tokens: number
  output_tokens: number
  cached_tokens: number
  /// cached / input, 0..1
  cache_ratio: number
  /// Mean latency of successful turns, ms. Null when none succeeded.
  avg_latency_ms: number | null
  cost_usd: number | null
}

export interface ProviderAggregate {
  provider: string
  requests: number
  input_tokens: number
  output_tokens: number
  cached_tokens: number
  cost_usd: number | null
  /// Busiest first.
  models: ModelAggregate[]
}

export interface StatsSummary {
  period_secs: number
  requests: number
  input_tokens: number
  output_tokens: number
  cached_tokens: number
  cache_ratio: number
  cost_usd: number
  per_provider: ProviderAggregate[]
}

export interface RequestEntry {
  ts: number
  provider: string
  model: string
  transport: string
  status: string
  error: string | null
  latency_ms: number | null
  input_tokens: number
  output_tokens: number
  cached_tokens: number
  cost_usd: number | null
}

export interface QuotaBar {
  label: string
  percent: number
  detail: string
}

export interface ProviderBalance {
  provider_id: string
  ok: boolean
  bars: QuotaBar[]
  balance_text?: string | null
  error?: string | null
}

export interface ProviderPreset {
  id: string
  name: string
  protocol: ProviderProtocol
  base_url: string
  defaultModels?: string[]
  userAgent?: string
}

// Mirrors src-tauri/src/providers.rs PRESETS.
export const PRESETS: ProviderPreset[] = [
  { id: 'kimi-coding', name: 'Kimi Code - Coding Plan', protocol: 'openai', base_url: 'https://api.kimi.com/coding/v1', defaultModels: ['k3', 'k3-256k', 'kimi-for-coding', 'kimi-for-coding-highspeed'], userAgent: 'KimiCLI/0.77' },
  { id: 'moonshot-global', name: 'Kimi API (Global)', protocol: 'openai', base_url: 'https://api.moonshot.ai/v1' },
  { id: 'moonshot-cn', name: 'Kimi API (China)', protocol: 'openai', base_url: 'https://api.moonshot.cn/v1' },
  { id: 'deepseek', name: 'DeepSeek', protocol: 'openai', base_url: 'https://api.deepseek.com/v1' },
  { id: 'openrouter', name: 'OpenRouter', protocol: 'openai', base_url: 'https://openrouter.ai/api/v1' },
  { id: 'groq', name: 'Groq', protocol: 'openai', base_url: 'https://api.groq.com/openai/v1' },
  { id: 'together', name: 'Together AI', protocol: 'openai', base_url: 'https://api.together.xyz/v1' },
  { id: 'mistral', name: 'Mistral AI', protocol: 'openai', base_url: 'https://api.mistral.ai/v1' },
  { id: 'siliconflow', name: 'SiliconFlow', protocol: 'openai', base_url: 'https://api.siliconflow.cn/v1' },
  { id: 'zai-coding', name: 'Z.ai GLM Coding Plan', protocol: 'openai', base_url: 'https://api.z.ai/api/coding/paas/v4' },
  { id: 'anthropic', name: 'Anthropic', protocol: 'anthropic', base_url: 'https://api.anthropic.com/v1' },
  { id: 'opencode-zen-chat', name: 'OpenCode Zen (Kimi/GLM/DeepSeek/MiniMax)', protocol: 'openai', base_url: 'https://opencode.ai/zen/v1', defaultModels: ['kimi-k3', 'kimi-k2.7-code', 'glm-5.2', 'deepseek-v4-pro', 'deepseek-v4-flash', 'minimax-m3'] },
  { id: 'opencode-zen-claude', name: 'OpenCode Zen (Claude/Qwen)', protocol: 'anthropic', base_url: 'https://opencode.ai/zen/v1', defaultModels: ['claude-sonnet-5', 'claude-opus-5', 'claude-haiku-4-5', 'qwen3.7-plus'] },
  { id: 'opencode-zen-responses', name: 'OpenCode Zen (GPT/Grok)', protocol: 'responses', base_url: 'https://opencode.ai/zen/v1', defaultModels: ['gpt-5.5', 'gpt-5.4-mini', 'gpt-5.4-nano', 'grok-4.5'] },
]
