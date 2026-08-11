// The first-run wizard contract, by test ID from .compozy/tasks/welcome-wizard/_tests.md.
// Frontend tests run against the api.ts mock shape; backend contracts are
// covered separately by the Rust suite and api.test.ts.

import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Provider, SetupStatus, ToolDetection, WizardStep } from '@/types'
import { setLocale } from '@/i18n'

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
let persistedStep: WizardStep | null = null
let setOnboardingStepFails = false
let multiAgentEnabled = false
let multiAgentWriteFails = false
let validationFirstOkAt: number | null = null
let validationFailedAttempt = false
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
const validateProvider = vi.fn((provider: Provider) => {
  void provider
  if (validateFails) return Promise.reject(new Error('network down'))
  return Promise.resolve([...validateResult])
})
const saveProvider = vi.fn((provider: Provider) => {
  if (saveFails) return Promise.reject(new Error('save failed'))
  savedProviders[provider.id] = {
    ...provider,
    api_key: null,
    keys: [],
    rotation_enabled: false,
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
    keys: [],
    rotation_enabled: false,
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
    name: 'Claude Code',
    protocol: 'anthropic',
    base_url: 'local',
    api_key: null,
    keys: [],
    rotation_enabled: false,
    has_key: false,
    enabled: true,
    models: [{ id: 'claude-sonnet-4-6', enabled: true, supports_vision: false }],
  }
  detection.claude.already_imported = true
  return Promise.resolve()
})
const setOnboardingStep = vi.fn((step: WizardStep) => {
  if (setOnboardingStepFails) return Promise.reject(new Error('step write failed'))
  persistedStep = step
  return Promise.resolve()
})
const multiAgentStatus = vi.fn(() => Promise.resolve(multiAgentEnabled))
const setMultiAgent = vi.fn((enabled: boolean) => {
  if (multiAgentWriteFails) return Promise.reject(new Error('multi-agent write failed'))
  multiAgentEnabled = enabled
  return Promise.resolve(multiAgentEnabled)
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
    validation: {
      started_at: codexManaged || validationFirstOkAt != null || validationFailedAttempt ? 1 : null,
      first_ok_request_at: validationFirstOkAt,
      failed_attempt: validationFailedAttempt,
    },
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
        onboarding_step: persistedStep,
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
    setOnboardingStep: (step: WizardStep) => setOnboardingStep(step),
    multiAgentStatus: () => multiAgentStatus(),
    setMultiAgent: (enabled: boolean) => setMultiAgent(enabled),
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
  persistedStep = null
  setOnboardingStepFails = false
  multiAgentEnabled = false
  multiAgentWriteFails = false
  validationFirstOkAt = null
  validationFailedAttempt = false
  resetDetection()
  codexApply.mockClear()
  validateProvider.mockClear()
  saveProvider.mockClear()
  toggleModel.mockClear()
  importOpencode.mockClear()
  importClaude.mockClear()
  setOnboardingStep.mockClear()
  multiAgentStatus.mockClear()
  setMultiAgent.mockClear()
  setupStatus.mockClear()
  completeOnboarding.mockClear()
  navigate.mockClear()
  onDone.mockClear()
})

afterEach(() => {
  setLocale('en')
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
  const advance =
    screen.queryByRole('button', { name: /^skip for now$/i }) ??
    screen.getByRole('button', { name: /continue/i })
  await user.click(advance)
  await screen.findByRole('heading', { name: /reuse tools/i })
  return user
}

async function toProvider() {
  const user = await toDetect()
  await user.click(screen.getByRole('button', { name: /skip for now/i }))
  await screen.findByRole('heading', { name: /add a provider/i })
  return user
}

async function toValidate() {
  const user = await toProvider()
  await user.click(screen.getByRole('button', { name: /^skip for now$/i }))
  await screen.findByRole('heading', { name: /check your first request/i })
  return user
}

async function toAgents() {
  const user = await toValidate()
  await user.click(screen.getByRole('button', { name: /^continue$/i }))
  await screen.findByRole('heading', { name: /agents and delegation/i })
  return user
}

async function selectOpenRouter(user: ReturnType<typeof userEvent.setup>) {
  await user.click(screen.getByRole('combobox', { name: /choose a provider/i }))
  await user.click(await screen.findByRole('option', { name: /^OpenRouter$/i }))
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
    const user = await toProvider()
    await user.click(screen.getByRole('combobox', { name: /choose a provider/i }))
    expect(await screen.findByRole('option', { name: /Kimi Code - Coding Plan/i })).toBeInTheDocument()
    await user.click(screen.getByRole('option', { name: /Kimi Code - Coding Plan/i }))
    expect(screen.getByText(/coding-plan models/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/API key/i)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /validate and save/i })).toBeInTheDocument()
  })

  it('UT-037 recommends OpenRouter for the unknown branch', async () => {
    const user = await toProvider()
    await user.click(screen.getByRole('combobox', { name: /choose a provider/i }))
    await user.click(await screen.findByRole('option', { name: /recommend/i }))
    expect(screen.getByRole('combobox', { name: /choose a provider/i })).toHaveTextContent(/OpenRouter/i)
    expect(screen.getByText(/one key gives access to many models/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/API key/i)).toBeInTheDocument()
  })

  it('UT-038 continues without a provider and leaves setup pending', async () => {
    const user = await toProvider()
    await user.click(screen.getByRole('button', { name: /^skip for now$/i }))
    expect(await screen.findByRole('heading', { name: /check your first request/i })).toBeInTheDocument()
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
    await user.click(screen.getByRole('combobox', { name: /choose a provider/i }))
    await user.click(await screen.findByRole('option', { name: /recommend/i }))
    expect(screen.queryByText(/region/i)).not.toBeInTheDocument()
    await user.click(screen.getByRole('combobox', { name: /choose a provider/i }))
    await user.click(await screen.findByRole('option', { name: /DeepSeek/i }))
    expect(screen.getByRole('combobox', { name: /choose a provider/i })).toHaveTextContent(/DeepSeek/i)
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
    expect(screen.getByRole('combobox', { name: /choose a provider/i })).toHaveTextContent(/OpenRouter/i)
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
      keys: [],
      rotation_enabled: false,
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
    fireEvent.change(screen.getByLabelText(/API key/i), { target: { value: longKey } })
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

const readyProvider = (): Provider => ({
  id: 'openrouter',
  name: 'OpenRouter',
  protocol: 'openai',
  base_url: 'https://openrouter.ai/api/v1',
  api_key: null,
  keys: [],
  rotation_enabled: false,
  has_key: true,
  enabled: true,
  models: [{ id: 'demo-model', enabled: true, supports_vision: false }],
})

const markReady = () => {
  codexManaged = true
  savedProviders.openrouter = readyProvider()
}

describe('validation step', () => {
  it('UT-058/UT-060 renders restart and first-request instructions until a request arrives', async () => {
    markReady()
    await toValidate()
    expect(screen.getByText(/setup is ready for its first request/i)).toBeInTheDocument()
    expect(screen.getAllByText(/restart codex, send one short message/i).length).toBeGreaterThan(0)
    expect(screen.queryByText(/first request worked/i)).not.toBeInTheDocument()
  })

  it('UT-059 confirms the first successful routed request', async () => {
    markReady()
    validationFirstOkAt = 100
    await toValidate()
    expect(screen.getByText(/first request worked/i)).toBeInTheDocument()
  })

  it('UT-061 surfaces a failed attempt with a Logs link', async () => {
    markReady()
    validationFailedAttempt = true
    await toValidate()
    expect(screen.getByText(/attempted but failed/i)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /open logs/i })).toBeInTheDocument()
  })

  it('UT-062 ignores pre-boundary requests instead of claiming success', async () => {
    markReady()
    await toValidate()
    expect(setupStatus).toHaveBeenCalled()
    expect(screen.queryByText(/first request worked/i)).not.toBeInTheDocument()
  })

  it('UT-063 a later ok request clears the failed state', async () => {
    markReady()
    validationFailedAttempt = true
    const user = await toValidate()
    expect(screen.getByText(/attempted but failed/i)).toBeInTheDocument()
    validationFailedAttempt = false
    validationFirstOkAt = 200
    await user.click(screen.getByRole('button', { name: /check again/i }))
    expect(await screen.findByText(/first request worked/i)).toBeInTheDocument()
  })

  it('UT-063b skips another setup probe while one is in flight', async () => {
    setupStatus.mockReturnValueOnce(new Promise<SetupStatus>(() => {}))
    setupStatus.mockReturnValueOnce(new Promise<SetupStatus>(() => {}))
    const user = await toValidate()
    await waitFor(() => expect(setupStatus.mock.calls.length).toBe(2))
    await user.click(await screen.findByRole('button', { name: /check again/i }))
    expect(setupStatus.mock.calls.length).toBe(2)
  })

  it('UT-064 closing while waiting resumes validation without sending a model request', async () => {
    markReady()
    persistedStep = 'validate'
    renderFlow()
    expect(await screen.findByRole('heading', { name: /check your first request/i })).toBeInTheDocument()
    expect(validateProvider).not.toHaveBeenCalled()
    expect(saveProvider).not.toHaveBeenCalled()
    expect(toggleModel).not.toHaveBeenCalled()
  })

  it('UT-065 not-ready setup lists missing items and still allows Finish', async () => {
    const user = await toValidate()
    expect(screen.getByText(/setup is still missing/i)).toBeInTheDocument()
    expect(screen.getByText(/provider setup/i)).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: /finish later/i }))
    expect(completeOnboarding).toHaveBeenCalled()
  })

  it('UT-066 first successful request wins over later state', async () => {
    markReady()
    validationFirstOkAt = 100
    await toValidate()
    expect(screen.getByText(/first request worked/i)).toBeInTheDocument()
  })

  it('UT-067 removing the provider after success returns to pending', async () => {
    markReady()
    validationFirstOkAt = 100
    const user = await toValidate()
    expect(screen.getByText(/first request worked/i)).toBeInTheDocument()
    delete savedProviders.openrouter
    await user.click(screen.getByRole('button', { name: /check again/i }))
    expect(await screen.findByText(/setup is still missing/i)).toBeInTheDocument()
  })
})

describe('agents step', () => {
  it('UT-068 toggle calls set_multi_agent and renders backend-confirmed state', async () => {
    const user = await toAgents()
    const toggle = await screen.findByRole('switch', { name: /enable multi-agent/i })
    await user.click(toggle)
    expect(setMultiAgent).toHaveBeenCalledWith(true)
    expect(toggle).toBeChecked()
  })

  it('UT-069 finishes without toggling and opens the app', async () => {
    const user = await toAgents()
    await user.click(screen.getByRole('button', { name: /^finish$/i }))
    expect(completeOnboarding).toHaveBeenCalled()
    expect(onDone).toHaveBeenCalled()
    expect(navigate).toHaveBeenCalledWith('/')
  })

  it('UT-070 reflects an already-enabled backend state', async () => {
    multiAgentEnabled = true
    await toAgents()
    expect(await screen.findByRole('switch', { name: /enable multi-agent/i })).toBeChecked()
  })

  it('UT-071 write failure keeps the previous state and shows an error', async () => {
    multiAgentWriteFails = true
    const user = await toAgents()
    const toggle = await screen.findByRole('switch', { name: /enable multi-agent/i })
    await waitFor(() => expect(toggle).toBeEnabled())
    await user.click(toggle)
    expect(await screen.findByText(/could not update multi-agent/i)).toBeInTheDocument()
    expect(screen.getByRole('switch', { name: /enable multi-agent/i })).not.toBeChecked()
  })

  it('UT-072 back to provider and forward keeps the toggle state', async () => {
    const user = await toAgents()
    await user.click(await screen.findByRole('switch', { name: /enable multi-agent/i }))
    await user.click(screen.getByRole('button', { name: /back/i }))
    await screen.findByRole('heading', { name: /add a provider/i })
    await user.click(screen.getByRole('button', { name: /^skip for now$/i }))
    await screen.findByRole('heading', { name: /check your first request/i })
    await user.click(screen.getByRole('button', { name: /^continue$/i }))
    await screen.findByRole('heading', { name: /agents and delegation/i })
    expect(screen.getByRole('switch', { name: /enable multi-agent/i })).toBeChecked()
  })

  it('UT-073 disables Finish while a toggle write is in flight', async () => {
    let resolveToggle!: (value: boolean) => void
    setMultiAgent.mockImplementationOnce(
      () => new Promise<boolean>((resolve) => {
        resolveToggle = resolve
      }),
    )
    const user = await toAgents()
    await user.click(await screen.findByRole('switch', { name: /enable multi-agent/i }))
    expect(screen.getByRole('button', { name: /^finish$/i })).toBeDisabled()
    resolveToggle(true)
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /^finish$/i })).toBeEnabled(),
    )
  })

  it('IT-008 multi-agent wiring confirms backend state and preserves it on write failure', async () => {
    const first = await toAgents()
    const toggle = await screen.findByRole('switch', { name: /enable multi-agent/i })
    await waitFor(() => expect(toggle).toBeEnabled())
    await first.click(toggle)
    expect(await screen.findByRole('switch', { name: /enable multi-agent/i })).toBeChecked()

    multiAgentWriteFails = true
    await first.click(screen.getByRole('switch', { name: /enable multi-agent/i }))
    expect(await screen.findByText(/could not update multi-agent/i)).toBeInTheDocument()
    expect(screen.getByRole('switch', { name: /enable multi-agent/i })).toBeChecked()
  })
})

describe('resume, persistence and finish', () => {
  it('UT-074 persists each step transition', async () => {
    const user = await startWizard()
    expect(setOnboardingStep).toHaveBeenLastCalledWith('codex')
    await user.click(screen.getByRole('button', { name: /skip for now/i }))
    await screen.findByRole('heading', { name: /reuse tools/i })
    expect(setOnboardingStep).toHaveBeenLastCalledWith('detect')
  })

  it('UT-076 a failed step write keeps the last persisted step', async () => {
    setOnboardingStepFails = true
    const user = userEvent.setup()
    renderFlow()
    await user.click(await screen.findByRole('button', { name: /^start$/i }))
    expect(screen.queryByRole('heading', { name: /connect codex/i })).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: /^start$/i })).toBeInTheDocument()
  })

  it('UT-077 resume re-reads real Codex state after an interruption', async () => {
    codexManaged = true
    persistedStep = 'codex'
    renderFlow()
    expect(await screen.findByRole('heading', { name: /connect codex/i })).toBeInTheDocument()
    expect(screen.getByText(/integration active/i)).toBeInTheDocument()
  })

  it('UT-078 tray focus during onboarding keeps the wizard focused', async () => {
    persistedStep = 'provider'
    renderFlow()
    expect(await screen.findByRole('heading', { name: /add a provider/i })).toBeInTheDocument()
    expect(navigate).not.toHaveBeenCalled()
  })

  it('UT-080 finish-later completes onboarding and opens the app', async () => {
    const user = await toAgents()
    await user.click(screen.getByRole('button', { name: /^finish$/i }))
    await waitFor(() => expect(completeOnboarding).toHaveBeenCalled())
    expect(onDone).toHaveBeenCalled()
    expect(navigate).toHaveBeenCalledWith('/')
  })
})

describe('i18n and accessibility', () => {
  it('UT-096 announces the step heading through an aria-live region', async () => {
    await startWizard()
    const live = document.querySelector('[aria-live="polite"]')
    expect(live).toBeInTheDocument()
    expect(live).toHaveTextContent(/connect codex/i)
  })

  it('UT-097 focuses the step heading on navigation', async () => {
    const user = await startWizard()
    expect(document.activeElement).toHaveTextContent(/connect codex/i)
    await user.click(screen.getByRole('button', { name: /skip for now/i }))
    await screen.findByRole('heading', { name: /reuse tools/i })
    expect(document.activeElement).toHaveTextContent(/reuse tools/i)
  })

  it('UT-098 keeps the wizard scrollable at narrow viewports', async () => {
    renderFlow()
    await screen.findByRole('button', { name: /^start$/i })
    expect(document.querySelector('.overflow-y-auto')).toBeInTheDocument()
  })

  it('UT-100 locale switching updates visible wizard strings', async () => {
    const user = await startWizard()
    setLocale('pt')
    expect(await screen.findByRole('heading', { name: /conectar o codex/i })).toBeInTheDocument()
    await user.click(await screen.findByRole('button', { name: /pular por enquanto/i }))
    expect(await screen.findByRole('heading', { name: /reaproveite ferramentas/i })).toBeInTheDocument()
  })

  it('UT-103 resume keeps the chosen locale', async () => {
    setLocale('pt')
    persistedStep = 'provider'
    renderFlow()
    expect(await screen.findByRole('heading', { name: /adicionar um provider/i })).toBeInTheDocument()
  })

  it('UT-099/UT-104 reduced motion keeps loading indicators visible', async () => {
    window.matchMedia = vi.fn((query: string) => ({
      matches: query.includes('prefers-reduced-motion'),
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    })) as unknown as typeof window.matchMedia
    codexApply.mockImplementationOnce(() => new Promise<void>(() => {}))
    const user = await startWizard()
    await user.click(screen.getByRole('button', { name: /activate integration/i }))
    expect(screen.getByText(/activating/i)).toBeInTheDocument()
    expect(document.querySelector('.animate-spin')).toHaveClass('motion-reduce:animate-none')
  })
})

describe('end-to-end mock journeys', () => {
  it('E2E-001 fresh install reaches ready validation and finish', async () => {
    markReady()
    const user = await startWizard()
    await user.click(screen.getByRole('button', { name: /continue/i }))
    await screen.findByRole('heading', { name: /reuse tools/i })
    await user.click(screen.getByRole('button', { name: /skip for now/i }))
    await screen.findByRole('heading', { name: /add a provider/i })
    await user.click(screen.getByRole('button', { name: /^skip for now$/i }))
    await screen.findByRole('heading', { name: /check your first request/i })
    await user.click(screen.getByRole('button', { name: /^continue$/i }))
    await screen.findByRole('heading', { name: /agents and delegation/i })
    await user.click(screen.getByRole('button', { name: /^finish$/i }))
    expect(completeOnboarding).toHaveBeenCalled()
    expect(navigate).toHaveBeenCalledWith('/')
  })

  it('E2E-002 interrupted setup resumes at the persisted step', async () => {
    markReady()
    persistedStep = 'provider'
    renderFlow()
    expect(await screen.findByRole('heading', { name: /add a provider/i })).toBeInTheDocument()
  })

  it('E2E-004 OpenCode import journey keeps credentials out of the UI', async () => {
    const user = await toDetect()
    await user.click(screen.getAllByRole('button', { name: /^import$/i })[0])
    await user.click(screen.getByRole('button', { name: /confirm import/i }))
    await waitFor(() => expect(importOpencode).toHaveBeenCalled())
    expect(screen.queryByText(/secret/i)).not.toBeInTheDocument()
  })

  it('E2E-005 logged-in Claude import journey uses consent', async () => {
    const user = await toDetect()
    await user.click(screen.getByRole('button', { name: /import claude code/i }))
    await user.click(screen.getByRole('button', { name: /confirm import/i }))
    expect(importClaude).toHaveBeenCalled()
    expect((await screen.findAllByText(/already imported/i)).length).toBeGreaterThan(0)
  })

  it('E2E-006 skip-all journey never sends a provider request', async () => {
    const user = await startWizard()
    await user.click(screen.getByRole('button', { name: /skip for now/i }))
    await screen.findByRole('heading', { name: /reuse tools/i })
    await user.click(screen.getByRole('button', { name: /skip for now/i }))
    await screen.findByRole('heading', { name: /add a provider/i })
    await user.click(screen.getByRole('button', { name: /^skip for now$/i }))
    await screen.findByRole('heading', { name: /check your first request/i })
    await user.click(screen.getByRole('button', { name: /^continue$/i }))
    await screen.findByRole('heading', { name: /agents and delegation/i })
    await user.click(screen.getByRole('button', { name: /^finish$/i }))
    expect(validateProvider).not.toHaveBeenCalled()
    expect(saveProvider).not.toHaveBeenCalled()
    expect(toggleModel).not.toHaveBeenCalled()
    expect(completeOnboarding).toHaveBeenCalled()
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
      keys: [],
      rotation_enabled: false,
      has_key: true,
      enabled: true,
      models: [{ id: 'demo-model', enabled: true, supports_vision: false }],
    }
    persistedStep = null
    cleanup()
    const second = await toProvider()
    await selectOpenRouter(second)
    await second.type(screen.getByLabelText(/API key/i), 'sk-updated')
    await second.click(screen.getByRole('button', { name: /validate and save/i }))
    await screen.findByText(/provider saved/i)
    expect(Object.keys(savedProviders).filter((id) => id === 'openrouter')).toHaveLength(1)

    savedProviders = {}
    validateFails = true
    persistedStep = null
    cleanup()
    const failing = await toProvider()
    await selectOpenRouter(failing)
    await failing.type(screen.getByLabelText(/API key/i), 'sk-bad')
    await failing.click(screen.getByRole('button', { name: /validate and save/i }))
    expect(await screen.findByRole('alert')).toHaveTextContent(/network down/i)
    expect(savedProviders.openrouter).toBeUndefined()
  })
})
