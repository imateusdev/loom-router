// API boundary: talks to the Rust backend via Tauri invoke.
// When running in a plain browser (bun run dev without Tauri), falls back
// to an in-memory mock so the UI stays previewable.

import type { AppConfig, CodexStatus, Provider, ServerStatus } from '@/types'

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
      mockState.config.providers[p.id] = p
      return Promise.resolve(undefined as T)
    }
    case 'delete_provider':
      delete mockState.config.providers[args?.id as string]
      return Promise.resolve(undefined as T)
    case 'discover_models':
      return Promise.resolve(['demo-model-small', 'demo-model-large'] as T)
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
  toggleModel: (providerId: string, model: string, enabled: boolean) =>
    call<void>('toggle_model', { providerId, model, enabled }),
  serverStatus: () => call<ServerStatus>('server_status'),
  serverStart: () => call<ServerStatus>('server_start'),
  serverStop: () => call<ServerStatus>('server_stop'),
  codexStatus: () => call<CodexStatus>('codex_status'),
  codexApply: () => call<void>('codex_apply'),
  codexRemove: () => call<void>('codex_remove'),
}

export { isTauri }
