import { describe, expect, it } from 'vitest'
import { PRESETS, type ProviderProtocol } from './index'

const PROTOCOLS: ProviderProtocol[] = ['openai', 'anthropic', 'responses']

describe('PRESETS', () => {
  it('gives each OpenCode gateway a single entry', () => {
    // One provider per subscription, not one per dialect: the six entries
    // this replaced all shared a URL and a key.
    const opencode = PRESETS.filter((p) => p.base_url.includes('opencode.ai'))
    expect(opencode.map((p) => p.id)).toEqual(['opencode-zen', 'opencode-go'])
    expect(opencode.map((p) => p.name)).toEqual(['OpenCode Zen', 'OpenCode Go'])
  })

  it('names the dialect of every model on a multi-dialect gateway', () => {
    // The dialect used to live on the provider, which is what the split was
    // for. Untagged here means the model would be sent to the wrong endpoint.
    for (const preset of PRESETS.filter((p) => p.base_url.includes('opencode.ai'))) {
      const models = preset.defaultModels ?? []
      expect(models.length).toBeGreaterThan(0)
      for (const m of models) {
        expect(Array.isArray(m), `${preset.id}: ${String(m)} carries no dialect`).toBe(true)
        expect(PROTOCOLS).toContain((m as [string, ProviderProtocol])[1])
      }
      // All three are in play; a gateway down to one would not need this.
      const dialects = new Set(models.map((m) => (m as [string, ProviderProtocol])[1]))
      expect([...dialects].sort()).toEqual(['anthropic', 'openai', 'responses'])
    }
  })

  it('keeps preset ids unique and models unrepeated', () => {
    const ids = PRESETS.map((p) => p.id)
    expect(new Set(ids).size).toBe(ids.length)
    for (const preset of PRESETS) {
      const models = (preset.defaultModels ?? []).map((m) => (typeof m === 'string' ? m : m[0]))
      expect(new Set(models).size, `${preset.id} repeats a model`).toBe(models.length)
    }
  })
})
