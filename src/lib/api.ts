// API boundary: talks to the Rust backend via Tauri invoke.
// When running in a plain browser (bun run dev without Tauri), falls back
// to an in-memory mock so the UI stays previewable.

import type { AppConfig, CodexStatus, Provider, ProviderBalance, RequestEntry, ServerStatus, StatsSummary } from '@/types'

const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (isTauri) {
    const { invoke } = await import('@tauri-apps/api/core')
    return invoke<T>(cmd, args)
  }
  return mock<T>(cmd, args)
}

// ---- Browser mock (dev preview only) ----

const mockState = {
  config: {
    port: 4180,
    autostart_server: false,
    providers: {
      deepseek: {
        id: 'deepseek',
        name: 'DeepSeek',
        protocol: 'openai',
        base_url: 'https://api.deepseek.com/v1',
        api_key: null,
        has_key: false,
        enabled: true,
        models: [
          { id: 'deepseek-chat', label: 'DeepSeek Chat', enabled: true },
          { id: 'deepseek-reasoner', label: null, enabled: false },
        ],
      },
    },
  } as AppConfig,
  running: false,
}

function mock<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  switch (cmd) {
    case 'get_config':
      return Promise.resolve(structuredClone(mockState.config) as T)
    case 'save_provider': {
      const p = args?.provider as Provider
      // Mirror the backend contract: "" means "keep the existing key",
      // a non-empty value stores a new key, and reads never return it.
      const existing = mockState.config.providers[p.id]
      const has_key = p.api_key ? true : (existing?.has_key ?? false)
      mockState.config.providers[p.id] = { ...p, api_key: null, has_key }
      return Promise.resolve(undefined as T)
    }
    case 'delete_provider':
      delete mockState.config.providers[args?.id as string]
      return Promise.resolve(undefined as T)
    case 'discover_models':
      return Promise.resolve(['demo-model-small', 'demo-model-large'] as T)
    case 'validate_provider': {
      const p = args?.provider as Provider
      const existing = mockState.config.providers[p.id]
      // Empty api_key means "use the stored key" (backend contract).
      if (!p.api_key && !existing?.has_key)
        return Promise.reject(new Error('API key is required'))
      return Promise.resolve(['demo-model-small', 'demo-model-large'] as T)
    }
    case 'toggle_model': {
      const { providerId, model, enabled } = args as {
        providerId: string
        model: string
        enabled: boolean
      }
      const prov = mockState.config.providers[providerId]
      const found = prov?.models.find((m) => m.id === model)
      if (found) found.enabled = enabled
      else prov?.models.push({ id: model, enabled })
      return Promise.resolve(undefined as T)
    }
    case 'server_status':
      return Promise.resolve({
        running: mockState.running,
        port: mockState.config.port,
        url: mockState.running ? `http://127.0.0.1:${mockState.config.port}/v1` : null,
      } as T)
    case 'server_start':
      mockState.running = true
      return mock('server_status')
    case 'server_stop':
      mockState.running = false
      return mock('server_status')
    case 'codex_status':
      return Promise.resolve({
        codex_home: '~/.codex',
        config_exists: true,
        managed_block_present: false,
        merged_catalog_present: false,
        merged_model_count: 1,
      } as T)
    case 'stats_summary':
      return Promise.resolve({
        period_secs: 86400,
        requests: 379,
        input_tokens: 2_000_000,
        output_tokens: 888_600,
        cached_tokens: 8_600_000,
        cache_ratio: 0.81,
        cost_usd: 14.72,
        per_provider: [
          { provider: 'kimi-coding', requests: 300, input_tokens: 1_700_000, output_tokens: 700_000, cached_tokens: 7_900_000, cost_usd: null },
          { provider: 'codex-native', requests: 79, input_tokens: 300_000, output_tokens: 188_600, cached_tokens: 700_000, cost_usd: 14.72 },
        ],
      } as T)
    case 'recent_requests':
      return Promise.resolve([
        { ts: 1_785_800_000, provider: 'kimi-coding', model: 'k3', transport: 'ws', status: 'ok', error: null, latency_ms: 1240, input_tokens: 12_400, output_tokens: 1_900, cached_tokens: 9_800, cost_usd: null },
        { ts: 1_785_799_000, provider: 'codex-native', model: 'gpt-5.5', transport: 'http', status: 'error', error: 'upstream returned 429', latency_ms: 310, input_tokens: 0, output_tokens: 0, cached_tokens: 0, cost_usd: null },
      ] as T)
    case 'provider_balances':
      return Promise.resolve([
        {
          provider_id: 'kimi-coding',
          ok: true,
          bars: [
            { label: 'Weekly quota', percent: 67, detail: '67 / 100 left · resets 2026-03-08T09:20' },
            { label: '5-hour window', percent: 93, detail: '93 / 100 left' },
          ],
          balance_text: null,
          error: null,
        },
      ] as T)
    default:
      return Promise.resolve(undefined as T)
  }
}

// ---- Public API ----

export const api = {
  getConfig: () => call<AppConfig>('get_config'),
  saveProvider: (provider: Provider) => call<void>('save_provider', { provider }),
  deleteProvider: (id: string) => call<void>('delete_provider', { id }),
  discoverModels: (providerId: string) => call<string[]>('discover_models', { providerId }),
  validateProvider: (provider: Provider) => call<string[]>('validate_provider', { provider }),
  toggleModel: (providerId: string, model: string, enabled: boolean) =>
    call<void>('toggle_model', { providerId, model, enabled }),
  serverStatus: () => call<ServerStatus>('server_status'),
  serverStart: () => call<ServerStatus>('server_start'),
  serverStop: () => call<ServerStatus>('server_stop'),
  codexStatus: () => call<CodexStatus>('codex_status'),
  codexApply: () => call<void>('codex_apply'),
  codexRemove: () => call<void>('codex_remove'),
  statsSummary: (periodSecs: number) => call<StatsSummary>('stats_summary', { periodSecs }),
  recentRequests: (limit?: number) => call<RequestEntry[]>('recent_requests', { limit }),
  providerBalances: () => call<ProviderBalance[]>('provider_balances'),
}

export { isTauri }
