// The browser mock is a contract, not a fixture: outside Tauri the whole UI
// runs against it, and every time it drifted from the backend something was
// unpreviewable or actively misleading. These tests pin the parts that have
// already drifted once.

import { describe, expect, it } from 'vitest'
import { api } from './api'
import type { Provider, VisualAssistanceConfig } from '@/types'

const provider = (over: Partial<Provider> = {}): Provider => ({
  id: 'acme',
  name: 'Acme',
  protocol: 'openai',
  base_url: 'https://api.acme.test/v1',
  api_key: null,
  has_key: false,
  user_agent: null,
  models: [],
  enabled: true,
  ...over,
})

describe('provider keys', () => {
  it('never hands a stored key back to the caller', async () => {
    await api.saveProvider(provider({ id: 'k1', api_key: 'secret-value' }))
    const config = await api.getConfig()
    expect(config.providers.k1.api_key).toBeNull()
    expect(config.providers.k1.has_key).toBe(true)
  })

  it('treats an empty key on save as "keep the existing one"', async () => {
    // The backend contract: the UI never receives the real key, so it cannot
    // send it back. Saving with "" must not wipe it.
    await api.saveProvider(provider({ id: 'k2', api_key: 'original' }))
    await api.saveProvider(provider({ id: 'k2', api_key: '', name: 'Renamed' }))
    const config = await api.getConfig()
    expect(config.providers.k2.has_key).toBe(true)
    expect(config.providers.k2.name).toBe('Renamed')
  })

  it('refuses to validate a provider with no key anywhere', async () => {
    await expect(api.validateProvider(provider({ id: 'k3' }))).rejects.toThrow()
  })
})

describe('codex integration', () => {
  it('reports the managed block only after apply, and drops it on remove', async () => {
    // A frozen status literal here meant no success state was previewable.
    await api.codexRemove()
    expect((await api.codexStatus()).managed_block_present).toBe(false)

    await api.codexApply()
    const applied = await api.codexStatus()
    expect(applied.managed_block_present).toBe(true)
    expect(applied.integration_enabled).toBe(true)

    await api.codexRemove()
    expect((await api.codexStatus()).managed_block_present).toBe(false)
  })

  it('returns a status object with every field of the type', async () => {
    // It used to omit three, which read as `undefined` at the call sites.
    const status = await api.codexStatus()
    for (const key of [
      'codex_home',
      'config_exists',
      'managed_block_present',
      'native_catalog_present',
      'merged_catalog_present',
      'merged_model_count',
      'codex_cli_available',
      'integration_enabled',
    ]) {
      expect(status).toHaveProperty(key)
      expect(status[key as keyof typeof status]).not.toBeUndefined()
    }
  })

  it('round-trips the multi-agent flag in both directions', async () => {
    // The UI once had no way to turn this off; the mock must not encode that.
    expect(await api.setMultiAgent(true)).toBe(true)
    expect(await api.setMultiAgent(false)).toBe(false)
  })
})

describe('wizard backend contracts', () => {
  it('UT-105 mirrors tool detection and both import commands', async () => {
    const before = await api.detectTools()
    expect(before.claude).toEqual({
      detected: expect.any(Boolean),
      logged_in: expect.any(Boolean),
      already_imported: expect.any(Boolean),
    })
    expect(before.opencode.config_found).toBe(true)
    expect(before.opencode.gateways.map((gateway) => gateway.id)).toEqual([
      'opencode-zen',
      'opencode-go',
    ])
    expect(JSON.stringify(before)).not.toContain('secret')

    await api.importOpencodeGateway('opencode-zen')
    await api.importClaudeCode()
    const after = await api.detectTools()
    expect(after.opencode.gateways[0].already_imported).toBe(true)
    expect(after.claude.already_imported).toBe(true)
  })

  it('persists a valid step, stamps validation once, and clears it on completion', async () => {
    await api.setOnboardingStep('provider')
    expect((await api.getConfig()).onboarding_step).toBe('provider')

    await api.setOnboardingStep('validate')
    const boundary = (await api.getConfig()).validation_started_at
    expect(typeof boundary).toBe('number')
    await api.setOnboardingStep('validate')
    expect((await api.getConfig()).validation_started_at).toBe(boundary)

    await api.completeOnboarding()
    expect((await api.getConfig()).onboarding_step).toBeNull()
  })

  it('returns the exact setup status shape and follows Codex state', async () => {
    await api.codexRemove()
    const pending = await api.setupStatus()
    expect(pending.ready).toBe(false)
    expect(pending.missing).toContain('codex_integration')
    expect(pending.validation).toEqual({
      started_at: expect.any(Number),
      first_ok_request_at: null,
      failed_attempt: false,
    })

    await api.codexApply()
    const ready = await api.setupStatus()
    expect(ready).toEqual({
      ready: true,
      missing: [],
      validation: pending.validation,
      codex_active: true,
    })
  })
})

describe('context windows', () => {
  it('mirrors the Kimi name rule instead of a flat fallback', async () => {
    const windows = await api.contextWindows()
    expect(windows['kimi-coding/k3']).toEqual({ window: 1_000_000, known: true })
    expect(windows['kimi-coding/k3-256k']).toEqual({ window: 262_144, known: true })
  })

  it('marks an unconfigured provider as a guess, not a 128k model', async () => {
    const windows = await api.contextWindows()
    expect(windows['deepseek/deepseek-chat']).toEqual({ window: 131_072, known: false })
  })
})

describe('visual assistance', () => {
  it('updates the preview config and model vision capability', async () => {
    const config: VisualAssistanceConfig = {
      enabled: true,
      assistant_model: 'deepseek/deepseek-chat',
      fallback_models: ['kimi-coding/k3'],
    }

    await api.setVisualAssistance(config)
    await api.setModelVision('deepseek', 'deepseek-chat', true)

    const saved = await api.getConfig()
    expect(saved.visual_assistance).toEqual(config)
    expect(saved.providers.deepseek.models[0].supports_vision).toBe(true)
  })
})

describe('usage summary', () => {
  it('breaks each provider down into its models', async () => {
    const summary = await api.statsSummary(86_400)
    expect(summary.per_provider.length).toBeGreaterThan(0)
    for (const p of summary.per_provider) {
      expect(Array.isArray(p.models)).toBe(true)
      expect(p.models.length).toBeGreaterThan(0)
      for (const m of p.models) {
        expect(typeof m.model).toBe('string')
        expect(typeof m.cache_ratio).toBe('number')
      }
    }
  })
})
