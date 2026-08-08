// The first-run wizard contract, by test ID from .compozy/tasks/welcome-wizard/_tests.md.
// Frontend tests run against the api.ts mock shape; backend contracts are
// covered separately by the Rust suite and api.test.ts.

import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { Provider, SetupStatus, ToolDetection } from '@/types'

let codexManaged = false
let codexCliAvailable = true
let applyFails = false
let applyWrites = true
let validateFails = false
let saveFails = false
let serverRejects = false
let serverPort = 4180
let validateResult: string[] = ['demo-model-small', 'demo-model-large']
let savedProviders: Record<string, Provider> = {}
let detection: ToolDetection = {
  claude: { detected: true, logged_in: true, already_imported: false },
  opencode: {
    config_found: true,
    gateways: [
      { id: 'opencode-zen', name: 'OpenCode Zen', importable: true, already_imported: false },
      { id: 'opencode-go', name: 'OpenCode Go', importable: true, already_imported: false },
    ],
  },
}

const codexApply = vi.fn(() => {
  if (applyFails) return Promise.reject(new Error('nope'))
  if (applyWrites) codexManaged = true
  return Promise.resolve()
})
const validateProvider = vi.fn(() => {
  if (validateFails) return Promise.reject(new Error('network down'))
  return Promise.resolve([...validateResult])
})
const saveProvider = vi.fn((provider: Provider) => {
  if (saveFails) return Promise.reject(new Error('save failed'))
  savedProviders[provider.id] = {
    ...provider,
    api_key: null,
    has_key: Boolean(provider.api_key),
  }
  return Promise.resolve()
})
const toggleModel = vi.fn((providerId: string, model: string, enabled: boolean) => {
  const provider = savedProviders[providerId]
  if (!provider) return Promise.reject(new Error('unknown provider'))
  provider.models = provider.models.map((m) => (m.id === model ? { ...m, enabled } : m))
  return Promise.resolve()
})
const importOpencode = vi.fn((id: 'opencode-zen' | 'opencode-go') => {
  savedProviders[id] = {
    id,
    name: id === 'opencode-zen' ? 'OpenCode Zen' : 'OpenCode Go',
    protocol: 'openai',
    base_url: id === 'opencode-zen' ? 'https://opencode.ai/zen/v1' : 'https://opencode.ai/zen/go/v1',
    api_key: null,
    has_key: true,
    enabled: true,
    models: [{ id: 'demo-model', enabled: true, supports_vision: false }],
  }
  const gateway = detection.opencode.gateways.find((g) => g.id === id)
  if (gateway) gateway.already_imported = true
  return Promise.resolve()
})
const importClaude = vi.fn(() => {
  savedProviders['claude-code'] = {
    id: 'claude-code',
    name: 'Claude Code (subscription)',
    protocol: 'anthropic',
    base_url: 'local',
    api_key: null,
    has_key: false,
    enabled: true,
    models: [{ id: 'claude-sonnet-4-6', enabled: true, supports_vision: false }],
  }
  detection.claude.already_imported = true
  return Promise.resolve()
})
const setupStatus = vi.fn((): Promise<SetupStatus> => {
  const credentialed = Object.values(savedProviders).filter(
    (provider) => provider.enabled && (provider.has_key || provider.id === 'claude-code'),
  )
  const providerReady = credentialed.some((provider) => provider.models.some((model) => model.enabled))
  const missing: SetupStatus['missing'] = []
  if (!codexManaged) missing.push('codex_integration')
  if (credentialed.length === 0) missing.push('provider')
  else if (!providerReady) missing.push('enabled_model')
  return Promise.resolve({
    ready: codexManaged && providerReady,
    missing,
    validation: { started_at: null, first_ok_request_at: null, failed_attempt: false },
    codex_active: codexManaged,
  })
})
const completeOnboarding = vi.fn(() => Promise.resolve())
const navigate = vi.fn()

vi.mock('react-router', () => ({ useNavigate: () => navigate }))
vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    serverStatus: () =>
      serverRejects
        ? Promise.reject(new Error('unavailable'))
        : Promise.resolve({ running: true, port: serverPort, url: null }),
    getConfig: () =>
      Promise.resolve({
        port: 4180,
        providers: savedProviders,
        side_call_fallback: null,
        visual_assistance: { enabled: false, assistant_model: null, fallback_models: [] },
        native_slug_mode: false,
        onboarding_completed: false,
        onboarding_step: null,
        validation_started_at: null,
      }),
    codexStatus: () =>
      Promise.resolve({
        codex_home: '~/.codex',
        config_exists: true,
        managed_block_present: codexManaged,
        managed_block_orphaned: false,
        native_catalog_present: codexManaged,
        merged_catalog_present: codexManaged,
        merged_model_count: codexManaged ? 1 : 0,
        codex_cli_available: codexCliAvailable,
        integration_enabled: codexManaged,
      }),
    codexApply: () => codexApply(),
    detectTools: () => Promise.resolve(structuredClone(detection)),
    importOpencodeGateway: (id: 'opencode-zen' | 'opencode-go') => importOpencode(id),
    importClaudeCode: () => importClaude(),
    setupStatus: () => setupStatus(),
    validateProvider: (provider: Provider) => validateProvider(provider),
    saveProvider: (provider: Provider) => saveProvider(provider),
    toggleModel: (providerId: string, model: string, enabled: boolean) =>
      toggleModel(providerId, model, enabled),
    completeOnboarding: () => completeOnboarding(),
  },
}))

import Onboarding from './Onboarding'

const onDone = vi.fn()
const renderFlow = () => render(<Onboarding onDone={onDone} />)

const resetDetection = () => {
  detection = {
    claude: { detected: true, logged_in: true, already_imported: false },
    opencode: {
      config_found: true,
      gateways: [
        { id: 'opencode-zen', name: 'OpenCode Zen', importable: true, already_imported: false },
        { id: 'opencode-go', name: 'OpenCode Go', importable: true, already_imported: false },
      ],
    },
  }
}

beforeEach(() => {
  codexManaged = false
  codexCliAvailable = true
  applyFails = false
  applyWrites = true
  validateFails = false
  saveFails = false
  serverRejects = false
  serverPort = 4180
  validateResult = ['demo-model-small', 'demo-model-large']
  savedProviders = {}
  resetDetection()
  codexApply.mockClear()
  validateProvider.mockClear()
  saveProvider.mockClear()
  toggleModel.mockClear()
  importOpencode.mockClear()
  importClaude.mockClear()
  setupStatus.mockClear()
  completeOnboarding.mockClear()
  navigate.mockClear()
  onDone.mockClear()
})

async function startWizard() {
  const user = userEvent.setup()
  renderFlow()
  await user.click(await screen.findByRole('button', { name: /^start$/i }))
  await screen.findByRole('heading', { name: /connect codex/i })
  return user
}

async function toDetect() {
  const user = await startWizard()
  await user.click(screen.getByRole('button', { name: /skip for now/i }))
  await screen.findByRole('heading', { name: /reuse tools/i })
  return user
}

async function toProvider() {
  const user = await toDetect()
  await user.click(screen.getByRole('button', { name: /skip for now/i }))
  await screen.findByRole('heading', { name: /add a provider/i })
  return user
}

async function selectOpenRouter(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByRole('button', { name: /^OpenRouter/i }))
}

async function saveOpenRouter(user: ReturnType<typeof userEvent.setup>) {
  await selectOpenRouter(user)
  await user.type(screen.getByLabelText(/API key/i), 'sk-test')
  await user.click(screen.getByRole('button', { name: /validate and save/i }))
  await screen.findByText(/provider saved/i)
}

describe('welcome and terminology', () => {
  it('UT-001 renders the name, one-sentence explanation and one primary Start', async () => {
    renderFlow()
    expect(await screen.findByRole('heading', { name: /LoomRouter/i })).toBeInTheDocument()
    expect(screen.getByText(/weave any model into your coding agent/i)).toBeInTheDocument()
    const start = screen.getByRole('button', { name: /^start$/i })
    expect(start).toHaveAttribute('data-variant', 'default')
    expect(screen.getAllByRole('button', { name: /^start$/i })).toHaveLength(1)
  })

  it('UT-002 omits the port badge when proxy status is unavailable', async () => {
    serverRejects = true
    renderFlow()
    const start = await screen.findByRole('button', { name: /^start$/i })
    expect(start).toBeEnabled()
    expect(screen.queryByText(/proxy already running/i)).not.toBeInTheDocument()
  })

  it('UT-003 gives plain-language explanations for each technical term', async () => {
    renderFlow()
    await screen.findByRole('button', { name: /^start$/i })
    expect(screen.getByRole('button', { name: /what is a provider/i })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /what is an api key/i })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /what is a model/i })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /what is the proxy/i })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /what is integration/i })).toBeInTheDocument()
  })

  it('UT-004 expands and collapses a detail with announced state', async () => {
    const user = userEvent.setup()
    renderFlow()
    const toggle = await screen.findByRole('button', { name: /what is a provider/i })
    expect(toggle).toHaveAttribute('aria-expanded', 'false')
    await user.click(toggle)
    expect(toggle).toHaveAttribute('aria-expanded', 'true')
    expect(screen.getByText(/a provider is the company/i)).toBeInTheDocument()
    await user.click(toggle)
    expect(toggle).toHaveAttribute('aria-expanded', 'false')
  })
})

describe('Codex step', () => {
  it('UT-005 shows active integration and the restart hint without Activate', async () => {
    codexManaged = true
    await startWizard()
    expect(screen.getByText(/integration active/i)).toBeInTheDocument()
    expect(screen.getByText(/reopen Codex afterwards/i)).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /activate integration/i })).not.toBeInTheDocument()
  })

  it('UT-006 activates once and only confirms after the backend read', async () => {
    const user = await startWizard()
    await user.click(screen.getByRole('button', { name: /activate integration/i }))
    expect(await screen.findByText(/integration active/i)).toBeInTheDocument()
    expect(codexApply).toHaveBeenCalledTimes(1)
  })

  it('UT-007 handles a missing Codex CLI with Retry and Skip', async () => {
    codexCliAvailable = false
    await startWizard()
    expect(screen.getByText(/not found on your PATH/i)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /try again/i })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /skip for now/i })).toBeInTheDocument()
  })

  it('UT-008 never reports false success when the read misses the block', async () => {
    applyWrites = false
    const user = await startWizard()
    await user.click(screen.getByRole('button', { name: /activate integration/i }))
    expect(await screen.findByText(/could not activate/i)).toBeInTheDocument()
    expect(screen.queryByText(/integration active/i)).not.toBeInTheDocument()
  })

  it('UT-009 double-click activates only once', async () => {
    await startWizard()
    const button = screen.getByRole('button', { name: /activate integration/i })
    fireEvent.click(button)
    fireEvent.click(button)
    await waitFor(() => expect(screen.getByText(/integration active/i)).toBeInTheDocument())
    expect(codexApply).toHaveBeenCalledTimes(1)
  })

  it('UT-010 skip before activation leaves Codex integration pending', async () => {
    const user = await startWizard()
    await user.click(screen.getByRole('button', { name: /skip for now/i }))
    await screen.findByRole('heading', { name: /reuse tools/i })
    await user.click(screen.getByRole('button', { name: /back/i }))
    expect(await screen.findByText(/not active yet/i)).toBeInTheDocument()
    expect(screen.getByText(/codex integration is not active yet/i)).toBeInTheDocument()
  })
})

describe('detect step', () => {
  it('offers consent before importing an OpenCode gateway', async () => {
    const user = await toDetect()
    await user.click(screen.getAllByRole('button', { name: /^import$/i })[0])
    expect(screen.getByText(/import OpenCode Zen/i)).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: /confirm import/i }))
    await waitFor(() => expect(importOpencode).toHaveBeenCalledWith('opencode-zen'))
    expect(await screen.findByText(/already imported/i)).toBeInTheDocument()
  })

  it('shows the manual path when nothing importable is found', async () => {
    detection = {
      claude: { detected: true, logged_in: false, already_imported: false },
      opencode: {
        config_found: true,
        gateways: [
          { id: 'opencode-zen', name: 'OpenCode Zen', importable: false, already_imported: false },
        ],
      },
    }
    await toDetect()
    expect(screen.getByText(/nothing reusable/i)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /set up a provider manually/i })).toBeInTheDocument()
  })
})

describe('provider step', () => {
  it('UT-036 lists common providers with one-sentence descriptions', async () => {
    await toProvider()
    expect(screen.getByRole('button', { name: /Kimi Code - Coding Plan/i })).toBeInTheDocument()
    expect(screen.getAllByText(/coding-plan models/i).length).toBeGreaterThan(0)
    await selectOpenRouter(await userEvent.setup())
    expect(screen.getByLabelText(/API key/i)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /validate and save/i })).toBeInTheDocument()
  })

  it('UT-037 recommends OpenRouter for the unknown branch', async () => {
    const user = await toProvider()
    await user.click(screen.getByRole('button', { name: /recommend/i }))
    expect(screen.getByRole('button', { name: /^OpenRouter/i })).toHaveAttribute('data-variant', 'default')
    expect(screen.getAllByText(/one key gives access to many models/i).length).toBeGreaterThan(0)
    expect(screen.getByLabelText(/API key/i)).toBeInTheDocument()
  })

  it('UT-038 continues without a provider and leaves setup pending', async () => {
    const user = await toProvider()
    await user.click(screen.getByRole('button', { name: /^skip for now$/i }))
    expect(await screen.findByRole('heading', { name: /next: first request check/i })).toBeInTheDocument()
  })

  it('UT-039 provider link failure keeps the wizard usable', async () => {
    const openSpy = vi.spyOn(window, 'open').mockImplementation(() => {
      throw new Error('blocked')
    })
    const user = await toProvider()
    await selectOpenRouter(user)
    await user.click(screen.getByRole('button', { name: /get an api key/i }))
    expect(openSpy).toHaveBeenCalled()
    expect(screen.getByText(/provider page could not be opened/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/API key/i)).toBeInTheDocument()
  })

  it('UT-040 recommendation copy avoids regional promises and keeps presets selectable', async () => {
    const user = await toProvider()
    await user.click(screen.getByRole('button', { name: /recommend/i }))
    expect(screen.queryByText(/region/i)).not.toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: /DeepSeek/i }))
    expect(screen.getByRole('button', { name: /DeepSeek/i })).toHaveAttribute('data-variant', 'default')
  })

  it('UT-041/UT-051 preserves provider selection and session key when navigating back', async () => {
    const user = await toProvider()
    await selectOpenRouter(user)
    await user.type(screen.getByLabelText(/API key/i), 'sk-session')
    await user.click(screen.getByRole('button', { name: /back/i }))
    await screen.findByRole('heading', { name: /reuse tools/i })
    await user.click(screen.getByRole('button', { name: /skip for now/i }))
    await screen.findByRole('heading', { name: /add a provider/i })
    expect(screen.getByLabelText(/API key/i)).toHaveValue('sk-session')
    expect(screen.getByRole('button', { name: /^OpenRouter/i })).toHaveAttribute('data-variant', 'default')
  })

  it('UT-042 validates and saves, then never returns the key to the UI', async () => {
    const user = await toProvider()
    await saveOpenRouter(user)
    expect(validateProvider).toHaveBeenCalledWith(expect.objectContaining({ api_key: 'sk-test' }))
    expect(saveProvider).toHaveBeenCalledWith(expect.objectContaining({ id: 'openrouter' }))
    expect(screen.queryByDisplayValue('sk-test')).not.toBeInTheDocument()
  })

  it('UT-043 explains key storage when the field is focused', async () => {
    const user = await toProvider()
    await selectOpenRouter(user)
    const input = screen.getByLabelText(/API key/i)
    expect(screen.queryByText(/stored only on this computer/i)).not.toBeInTheDocument()
    await user.click(input)
    expect(screen.getByText(/stored only on this computer/i)).toBeInTheDocument()
  })

  it('UT-044 validation failure keeps the key field focused and offers Retry', async () => {
    validateFails = true
    const user = await toProvider()
    await selectOpenRouter(user)
    const input = screen.getByLabelText(/API key/i)
    await user.type(input, 'sk-bad')
    await user.click(screen.getByRole('button', { name: /validate and save/i }))
    expect(await screen.findByRole('alert')).toHaveTextContent(/network down/i)
    expect(input).toHaveFocus()
    expect(screen.getByRole('button', { name: /try again/i })).toBeInTheDocument()
  })

  it('UT-045 backend save error never renders a success state', async () => {
    saveFails = true
    const user = await toProvider()
    await selectOpenRouter(user)
    await user.type(screen.getByLabelText(/API key/i), 'sk-test')
    await user.click(screen.getByRole('button', { name: /validate and save/i }))
    expect(await screen.findByRole('alert')).toHaveTextContent(/save failed/i)
    expect(screen.queryByText(/provider saved/i)).not.toBeInTheDocument()
  })

  it('UT-046 empty key blocks save with a plain-language message', async () => {
    const user = await toProvider()
    await selectOpenRouter(user)
    await user.click(screen.getByRole('button', { name: /validate and save/i }))
    expect(await screen.findByRole('alert')).toHaveTextContent(/enter an api key first/i)
    expect(saveProvider).not.toHaveBeenCalled()
  })

  it('UT-047 provider network failure never shows a false valid state', async () => {
    validateFails = true
    const user = await toProvider()
    await selectOpenRouter(user)
    await user.type(screen.getByLabelText(/API key/i), 'sk-bad')
    await user.click(screen.getByRole('button', { name: /validate and save/i }))
    expect(await screen.findByRole('alert')).toHaveTextContent(/network down/i)
    expect(screen.queryByText(/provider saved/i)).not.toBeInTheDocument()
  })

  it('UT-048 endpoint with no models explains and supports Save anyway', async () => {
    validateResult = []
    const user = await toProvider()
    await selectOpenRouter(user)
    await user.type(screen.getByLabelText(/API key/i), 'sk-test')
    await user.click(screen.getByRole('button', { name: /validate and save/i }))
    expect(await screen.findByRole('alert')).toHaveTextContent(/no models are configured/i)
    await user.click(screen.getByRole('button', { name: /save anyway/i }))
    expect(await screen.findByText(/provider saved/i)).toBeInTheDocument()
  })

  it('UT-049 saving the same id updates instead of duplicating', async () => {
    savedProviders.openrouter = {
      id: 'openrouter',
      name: 'OpenRouter',
      protocol: 'openai',
      base_url: 'https://openrouter.ai/api/v1',
      api_key: null,
      has_key: true,
      enabled: true,
      models: [{ id: 'demo-model', enabled: true, supports_vision: false }],
    }
    const user = await toProvider()
    await selectOpenRouter(user)
    await user.type(screen.getByLabelText(/API key/i), 'sk-updated')
    await user.click(screen.getByRole('button', { name: /validate and save/i }))
    await screen.findByText(/provider saved/i)
    expect(Object.keys(savedProviders).filter((id) => id === 'openrouter')).toHaveLength(1)
    expect(savedProviders.openrouter.has_key).toBe(true)
  })

  it('UT-050 a very long key remains editable without breaking the layout', async () => {
    const user = await toProvider()
    await selectOpenRouter(user)
    const longKey = 'sk-'.padEnd(512, 'x')
    await user.type(screen.getByLabelText(/API key/i), longKey)
    expect(screen.getByLabelText(/API key/i)).toHaveValue(longKey)
    expect(screen.getByRole('button', { name: /validate and save/i })).toBeEnabled()
  })
})

describe('model enablement', () => {
  it('UT-052 renders an enabled model count after save', async () => {
    const user = await toProvider()
    await saveOpenRouter(user)
    expect(screen.getByText(/provider saved/i)).toBeInTheDocument()
    expect(screen.getByText(/models enabled/i)).toBeInTheDocument()
  })

  it('UT-053 toggling updates the count immediately and confirms with the backend', async () => {
    const user = await toProvider()
    await saveOpenRouter(user)
    const model = screen.getByRole('switch', { name: 'demo-model-small' })
    await user.click(model)
    expect(screen.getByText(/0 models enabled/i)).toBeInTheDocument()
    expect(toggleModel).toHaveBeenCalledWith('openrouter', 'demo-model-small', false)
  })

  it('UT-054 empty model discovery keeps existing provider models visible', async () => {
    validateResult = []
    const user = await toProvider()
    await selectOpenRouter(user)
    await user.type(screen.getByLabelText(/API key/i), 'sk-test')
    await user.click(screen.getByRole('button', { name: /validate and save/i }))
    await user.click(await screen.findByRole('button', { name: /save anyway/i }))
    expect(await screen.findByText(/provider saved/i)).toBeInTheDocument()
    expect(screen.getByText(/no models are configured for this provider/i)).toBeInTheDocument()
  })

  it('UT-055 rapid same-model toggles settle to the last confirmed state', async () => {
    const user = await toProvider()
    await saveOpenRouter(user)
    const model = screen.getByRole('switch', { name: 'demo-model-small' })
    await user.click(model)
    await user.click(model)
    expect(toggleModel.mock.calls.filter(([, id]) => id === 'demo-model-small')).toHaveLength(2)
    expect(toggleModel).toHaveBeenLastCalledWith('openrouter', 'demo-model-small', true)
  })

  it('UT-056/UT-057 all models disabled makes provider pending and re-enabling restores readiness', async () => {
    const user = await toProvider()
    await saveOpenRouter(user)
    await user.click(screen.getByRole('switch', { name: 'demo-model-small' }))
    const pending = await setupStatus()
    expect(pending.missing).toContain('enabled_model')
    expect(pending.ready).toBe(false)
    await user.click(screen.getByRole('switch', { name: 'demo-model-large' }))
    const ready = await setupStatus()
    expect(ready.missing).not.toContain('enabled_model')
  })
})

describe('wizard backend contract', () => {
  it('IT-005 provider create/update and validation failure through the wizard', async () => {
    const user = await toProvider()
    await saveOpenRouter(user)
    expect(savedProviders.openrouter).toBeDefined()

    savedProviders.openrouter = {
      id: 'openrouter',
      name: 'OpenRouter',
      protocol: 'openai',
      base_url: 'https://openrouter.ai/api/v1',
      api_key: null,
      has_key: true,
      enabled: true,
      models: [{ id: 'demo-model', enabled: true, supports_vision: false }],
    }
    cleanup()
    const second = await toProvider()
    await selectOpenRouter(second)
    await second.type(screen.getByLabelText(/API key/i), 'sk-updated')
    await second.click(screen.getByRole('button', { name: /validate and save/i }))
    await screen.findByText(/provider saved/i)
    expect(Object.keys(savedProviders).filter((id) => id === 'openrouter')).toHaveLength(1)

    savedProviders = {}
    validateFails = true
    cleanup()
    const failing = await toProvider()
    await selectOpenRouter(failing)
    await failing.type(screen.getByLabelText(/API key/i), 'sk-bad')
    await failing.click(screen.getByRole('button', { name: /validate and save/i }))
    expect(await screen.findByRole('alert')).toHaveTextContent(/network down/i)
    expect(savedProviders.openrouter).toBeUndefined()
  })
})
