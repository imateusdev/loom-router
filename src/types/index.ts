// LoomRouter shared types (mirror of src-tauri Rust structs).

export type ProviderProtocol = 'openai' | 'anthropic' | 'responses'

export interface ProviderModel {
  id: string
  label?: string | null
  // Real window learned during discovery; absent means "fall back to
  // heuristics" (mirrors codex::context_window_for precedence).
  context_window?: number | null
  // Wire dialect for this one model, when the gateway serves it in a
  // different one than the rest of its catalog (OpenCode speaks three behind
  // a single URL). Absent means "whatever the provider speaks".
  protocol?: ProviderProtocol | null
  // True only on the claude-code provider's Opus models: the model
  // participates in Claude Code fast mode (`/fast`).
  fast_mode?: boolean
  enabled: boolean
  // Whether the model accepts image input for visual assistance.
  supports_vision: boolean
}

export interface VisualAssistanceConfig {
  enabled: boolean
  assistant_model: string | null
  // Ordered `provider/model` slugs tried after assistant_model.
  fallback_models: string[]
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
  // Provider-wide window override; per-model values (ProviderModel) win.
  context_window?: number | null
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
  visual_assistance: VisualAssistanceConfig
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
  onboarding_step?: WizardStep | null
  // Unix seconds marking the first visit to the validation step.
  validation_started_at?: number | null
}

export type WizardStep =
  | 'welcome'
  | 'codex'
  | 'detect'
  | 'provider'
  | 'validate'
  | 'agents'
  | 'finish'

export interface SetupStatus {
  ready: boolean
  missing: Array<'codex_integration' | 'provider' | 'enabled_model'>
  validation: {
    started_at: number | null
    first_ok_request_at: number | null
    failed_attempt: boolean
  }
  codex_active: boolean
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
  /// Grouping slug: review | build | investigate | quality | ship | write |
  /// data | ops. Translated in the UI.
  category: string
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

/// Login state of the local `claude` CLI, backing the claude-code provider.
/// Mirrors src-tauri/src/claude_cli.rs.
export interface ClaudeAuthStatus {
  logged_in: boolean
  auth_method?: string | null
  subscription_type?: string | null
  email?: string | null
  plan?: string | null
  error?: string | null
}

export interface CodexStatus {
  codex_home: string
  config_exists: boolean
  managed_block_present: boolean
  managed_block_orphaned: boolean
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
  visual_assistance?: VisualAssistanceMetadata
  cost_usd: number | null
}

export interface VisualAssistanceMetadata {
  images: VisualImageProvenance[]
  // Absent in rows persisted by older versions.
  attempts?: VisualAttemptProvenance[]
}

export interface VisualImageProvenance {
  model: string
  attempts: number
  duration_ms: number
  cache_hit: boolean
}

export interface VisualAttemptProvenance {
  model: string
  retryable: boolean
  status: number | null
  duration_ms: number
  error: string
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
  // Seeded models. A plain string takes the preset's own protocol; the
  // tuple form names the dialect the gateway serves that model in.
  defaultModels?: (string | [string, ProviderProtocol])[]
  userAgent?: string
}

// Mirrors src-tauri/src/providers.rs PRESETS.
export const PRESETS: ProviderPreset[] = [
  { id: 'claude-code', name: 'Claude Code (subscription)', protocol: 'anthropic', base_url: 'local', defaultModels: ['claude-fable-5', 'claude-opus-5', 'claude-opus-4-8', 'claude-sonnet-4-6', 'claude-haiku-4-5'] },
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
  // One gateway per subscription, three dialects behind each — so the
  // dialect travels with the model, not with the provider.
  {
    id: 'opencode-zen', name: 'OpenCode Zen', protocol: 'openai', base_url: 'https://opencode.ai/zen/v1',
    defaultModels: [
      ['kimi-k3', 'openai'], ['kimi-k2.7-code', 'openai'], ['glm-5.2', 'openai'],
      ['deepseek-v4-pro', 'openai'], ['deepseek-v4-flash', 'openai'], ['minimax-m3', 'openai'],
      ['claude-sonnet-5', 'anthropic'], ['claude-opus-5', 'anthropic'],
      ['claude-haiku-4-5', 'anthropic'], ['qwen3.7-plus', 'anthropic'],
      ['gpt-5.5', 'responses'], ['gpt-5.4-mini', 'responses'],
      ['gpt-5.4-nano', 'responses'], ['grok-4.5', 'responses'],
    ],
  },
  {
    id: 'opencode-go', name: 'OpenCode Go', protocol: 'openai', base_url: 'https://opencode.ai/zen/go/v1',
    defaultModels: [
      ['kimi-k3', 'openai'], ['kimi-k2.7-code', 'openai'], ['glm-5.2', 'openai'],
      ['deepseek-v4-pro', 'openai'], ['deepseek-v4-flash', 'openai'],
      ['mimo-v2.5-pro', 'openai'], ['hy3', 'openai'],
      ['minimax-m3', 'anthropic'], ['minimax-m2.7', 'anthropic'],
      ['qwen3.8-max', 'anthropic'], ['qwen3.7-max', 'anthropic'],
      ['qwen3.7-plus', 'anthropic'], ['qwen3.6-plus', 'anthropic'],
      ['gpt-5.6-luna', 'responses'],
    ],
  },
]
