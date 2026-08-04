// LoomRouter shared types (mirror of src-tauri Rust structs).

export type ProviderProtocol = 'openai' | 'anthropic'

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
  api_key?: string | null
  models: ProviderModel[]
  enabled: boolean
}

export interface AppConfig {
  port: number
  providers: Record<string, Provider>
  autostart_server: boolean
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
}

export interface ProviderPreset {
  id: string
  name: string
  protocol: ProviderProtocol
  base_url: string
  defaultModels?: string[]
}

// Mirrors src-tauri/src/providers.rs PRESETS.
export const PRESETS: ProviderPreset[] = [
  { id: 'kimi-coding', name: 'Kimi Code - Coding Plan', protocol: 'openai', base_url: 'https://api.kimi.com/coding/v1', defaultModels: ['kimi-for-coding'] },
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
]
