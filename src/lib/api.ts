// API boundary: talks to the Rust backend via Tauri invoke.
// When running in a plain browser (bun run dev without Tauri), falls back
// to an in-memory mock so the UI stays previewable.

import type { AgentInfo, AgentTemplate, AppConfig, CodexStatus, Provider, ProviderBalance, RequestEntry, ServerStatus, StatsSummary } from '@/types'

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
    side_call_fallback: null,
    native_slug_mode: false,
    // The browser preview shows the app itself; flip this to undefined to
    // preview the first-run walkthrough without a fresh install.
    onboarding_completed: true,
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
  codexApplied: false,
  agents: [
    {
      name: 'reviewer',
      description: 'Use for read-only code review focused on correctness and missing tests.',
      model: 'deepseek/deepseek-chat',
      effort: 'high',
      sandbox_mode: 'read-only',
      instructions: 'Review code changes for correctness, security and style.',
    },
    {
      name: 'writer',
      description: 'Use for documentation and changelog drafting.',
      model: null,
      effort: null,
      sandbox_mode: null,
      instructions: 'Draft documentation and changelogs from diffs.',
    },
  ] as AgentInfo[],
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
    case 'complete_onboarding':
      mockState.config.onboarding_completed = true
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
    case 'agents_list':
      return Promise.resolve(structuredClone(mockState.agents) as T)
    case 'agents_upsert': {
      const agent = args?.agent as AgentInfo
      const idx = mockState.agents.findIndex((a) => a.name === agent.name)
      if (idx >= 0) mockState.agents[idx] = agent
      else mockState.agents.push(agent)
      return Promise.resolve(undefined as T)
    }
    case 'agents_delete':
      mockState.agents = mockState.agents.filter((a) => a.name !== (args?.name as string))
      return Promise.resolve(undefined as T)
    case 'agent_templates':
      return Promise.resolve([
        {
          id: 'reviewer',
          label: 'Reviewer',
          blurb: 'Read-only code review: correctness, regressions, missing tests.',
          description: 'Use for read-only code review focused on correctness, regressions, edge cases, and missing tests.',
          instructions: 'You are a code reviewer. Stay read-only.\n\nReview the changes you are given like an owner.',
          sandbox_mode: 'read-only',
        },
      ] as T)
    case 'multi_agent_status':
      return Promise.resolve(true as T)
    case 'set_multi_agent':
      // Parenthesised deliberately: `x ?? true as T` binds as
      // `x ?? (true as T)`, which types the expression `boolean | T` and
      // fails the build.
      return Promise.resolve(((args?.enabled as boolean) ?? true) as T)
    case 'set_side_call_fallback':
      mockState.config.side_call_fallback = (args?.model as string | null) ?? null
      return Promise.resolve(undefined as T)
    case 'set_native_slug_mode':
      mockState.config.native_slug_mode = (args?.enabled as boolean) ?? false
      return Promise.resolve(undefined as T)
    // Mirror the backend contract: apply/remove flip the managed block, and
    // status reports it. Previously status was a frozen literal missing
    // three fields of CodexStatus (hidden by the `as T` cast), so anything
    // reading them — the walkthrough's "integration active" check — saw
    // undefined and could never show a success state in the preview.
    case 'codex_status':
      return Promise.resolve({
        codex_home: '~/.codex',
        config_exists: true,
        managed_block_present: mockState.codexApplied,
        native_catalog_present: mockState.codexApplied,
        merged_catalog_present: mockState.codexApplied,
        merged_model_count: mockState.codexApplied ? 1 : 0,
        codex_cli_available: true,
        integration_enabled: mockState.codexApplied,
      } as T)
    case 'codex_apply':
      mockState.codexApplied = true
      return Promise.resolve(undefined as T)
    case 'codex_remove':
      mockState.codexApplied = false
      return Promise.resolve(undefined as T)
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
  agentsList: () => call<AgentInfo[]>('agents_list'),
  agentsUpsert: (agent: AgentInfo) => call<void>('agents_upsert', { agent }),
  agentsDelete: (name: string) => call<void>('agents_delete', { name }),
  agentTemplates: () => call<AgentTemplate[]>('agent_templates'),
  multiAgentStatus: () => call<boolean>('multi_agent_status'),
  setMultiAgent: (enabled: boolean) => call<boolean>('set_multi_agent', { enabled }),
  setSideCallFallback: (model: string | null) => call<void>('set_side_call_fallback', { model }),
  setNativeSlugMode: (enabled: boolean) => call<void>('set_native_slug_mode', { enabled }),
  completeOnboarding: () => call<void>('complete_onboarding'),
  statsSummary: (periodSecs: number) => call<StatsSummary>('stats_summary', { periodSecs }),
  recentRequests: (limit?: number) => call<RequestEntry[]>('recent_requests', { limit }),
  providerBalances: () => call<ProviderBalance[]>('provider_balances'),
}

export { isTauri }
