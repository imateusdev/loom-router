// Regression cover for a one-way door: the Agents page showed a multi-agent
// prompt only while the feature was off, so enabling it removed the only
// control that existed. The backend always accepted `false`; nothing in the
// UI ever called it. The switch must be reachable in both states.

import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'

let multiAgent = false
let orphaned = false
let demoProviderHasKey = true
let visualAssistance = {
  enabled: false,
  assistant_model: 'demo/vision-primary' as string | null,
  fallback_models: [] as string[],
}
const setMultiAgent = vi.fn((next: boolean) => {
  multiAgent = next
  return Promise.resolve(next)
})
const setVisualAssistance = vi.fn((next: typeof visualAssistance) => {
  visualAssistance = next
  return Promise.resolve()
})
const setNativeModelContextOverride = vi.fn<
  (model: string, contextWindow: number) => Promise<void>
>(() => Promise.resolve())
const clearNativeModelContextOverride = vi.fn<(model: string) => Promise<void>>(() =>
  Promise.resolve(),
)
let nativeContextOverrides: Record<string, number> = {}

vi.mock('@/lib/events', () => ({ useBackendState: () => {} }))

vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    codexNativeModels: () => Promise.resolve(['gpt-5.6-sol', 'gpt-5.6-terra']),
    codexStatus: () =>
      Promise.resolve({
        codex_home: '~/.codex',
        config_exists: true,
        config_parseable: true,
        managed_block_present: true,
        managed_block_orphaned: orphaned,
        native_catalog_present: true,
        merged_catalog_present: true,
        merged_model_count: 3,
        codex_cli_available: true,
        codex_config_loads: true,
        codex_config_error: null,
        integration_enabled: true,
        session: {
          path: '~/.codex/auth.json',
          present: false,
          usable: false,
          has_account_id: false,
          expired: false,
          expires_in_hours: null,
          age_hours: null,
        },
      }),
    getConfig: () =>
      Promise.resolve({
        port: 4180,
        providers: {
          demo: {
            id: 'demo',
            name: 'Demo',
            protocol: 'openai',
            base_url: 'https://example.test',
            has_key: demoProviderHasKey,
            enabled: true,
            models: [
              { id: 'vision-primary', label: 'Vision primary', enabled: true, supports_vision: true },
              { id: 'vision-fallback-a', label: 'Vision fallback A', enabled: true, supports_vision: true },
              { id: 'vision-fallback-b', label: 'Vision fallback B', enabled: true, supports_vision: true },
              { id: 'vision-catalog-only', label: 'Vision catalog only', enabled: false, supports_vision: true },
              { id: 'vision-responses', label: 'Vision responses', enabled: true, supports_vision: true, protocol: 'responses' },
              { id: 'text-only', label: 'Text only', enabled: true, supports_vision: false },
            ],
          },
        },
        side_call_fallback: null,
        visual_assistance: visualAssistance,
        native_slug_mode: false,
        native_model_context_overrides: nativeContextOverrides,
      }),
    multiAgentStatus: () => Promise.resolve(multiAgent),
    setMultiAgent: (v: boolean) => setMultiAgent(v),
    codexApply: () => Promise.resolve(),
    codexRemove: () => Promise.resolve(),
    setSideCallFallback: () => Promise.resolve(),
    setNativeSlugMode: () => Promise.resolve(),
    setNativeModelContextOverride: (model: string, contextWindow: number) =>
      setNativeModelContextOverride(model, contextWindow),
    clearNativeModelContextOverride: (model: string) => clearNativeModelContextOverride(model),
    setVisualAssistance: (v: typeof visualAssistance) => setVisualAssistance(v),
  },
}))

import CodexPage from './Codex'

const multiAgentSwitch = async () => {
  const card = (await screen.findByText(/^Multi-agent$/)).closest('div[class*="rounded"]')
  return within(card as HTMLElement).getByRole('switch')
}

describe('multi-agent control', () => {
  beforeEach(() => {
    orphaned = false
    visualAssistance = {
      enabled: false,
      assistant_model: 'demo/vision-primary',
      fallback_models: [],
    }
    vi.clearAllMocks()
  })

  it('is reachable and turns off again once on', async () => {
    multiAgent = true
    const user = userEvent.setup()
    render(<CodexPage />)

    const toggle = await multiAgentSwitch()
    await waitFor(() => expect(toggle).toBeChecked())

    // The bug: with only the "enable" banner, there was no control here at
    // all once the feature was on.
    await user.click(toggle)
    expect(setMultiAgent).toHaveBeenCalledWith(false)
    await waitFor(() => expect(toggle).not.toBeChecked())
  })

  it('turns on from the off state', async () => {
    multiAgent = false
    const user = userEvent.setup()
    render(<CodexPage />)

    const toggle = await multiAgentSwitch()
    await waitFor(() => expect(toggle).not.toBeChecked())

    await user.click(toggle)
    expect(setMultiAgent).toHaveBeenCalledWith(true)
    await waitFor(() => expect(toggle).toBeChecked())
  })

  it('shows the state the backend reports, not the one requested', async () => {
    // A failed write must not leave the switch lying about the config.
    multiAgent = false
    setMultiAgent.mockImplementationOnce(() => Promise.resolve(false))
    const user = userEvent.setup()
    render(<CodexPage />)

    const toggle = await multiAgentSwitch()
    await user.click(toggle)
    await waitFor(() => expect(toggle).not.toBeChecked())
  })
})

describe('native context override', () => {
  beforeEach(() => {
    nativeContextOverrides = {}
    vi.clearAllMocks()
  })

  it('saves a persistent context window for the selected native model', async () => {
    const user = userEvent.setup()
    render(<CodexPage />)

    const title = await screen.findByText('Native context override')
    const card = title.closest('div[class*="rounded"]') as HTMLElement
    const input = within(card).getByRole('spinbutton', { name: 'Context window tokens' })
    await user.clear(input)
    await user.type(input, '1000000')
    await user.click(within(card).getByRole('button', { name: 'Save override' }))

    expect(setNativeModelContextOverride).toHaveBeenCalledWith('gpt-5.6-sol', 1_000_000)
  })

  it('returns a model to the Codex catalog default', async () => {
    nativeContextOverrides = { 'gpt-5.6-sol': 1_000_000 }
    const user = userEvent.setup()
    render(<CodexPage />)

    const title = await screen.findByText('Native context override')
    const card = title.closest('div[class*="rounded"]') as HTMLElement
    expect(within(card).getByRole('spinbutton')).toHaveValue(1_000_000)
    await user.click(within(card).getByRole('button', { name: 'Use Codex default' }))

    expect(clearNativeModelContextOverride).toHaveBeenCalledWith('gpt-5.6-sol')
  })
})

describe('visual assistance settings', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    demoProviderHasKey = true
  })

  it('enables and disables visual assistance', async () => {
    const user = userEvent.setup()
    render(<CodexPage />)

    const toggle = await screen.findByRole('switch', { name: /visual assistance/i })
    const primary = screen.getByRole('combobox', { name: /primary visual assistant/i })
    // The primary picker stays selectable while assistance is off so the
    // model can be chosen before the feature is switched on.
    expect(primary).not.toBeDisabled()
    await user.click(toggle)
    await waitFor(() =>
      expect(setVisualAssistance).toHaveBeenLastCalledWith({
        enabled: true,
        assistant_model: 'demo/vision-primary',
        fallback_models: [],
      }),
    )
    expect(primary).not.toBeDisabled()

    await user.click(toggle)
    await waitFor(() =>
      expect(setVisualAssistance).toHaveBeenLastCalledWith({
        enabled: false,
        assistant_model: 'demo/vision-primary',
        fallback_models: [],
      }),
    )
  })

  it('saves a vision primary and ordered explicit fallbacks', async () => {
    visualAssistance = { enabled: true, assistant_model: null, fallback_models: [] }
    const user = userEvent.setup()
    render(<CodexPage />)

    await user.click(await screen.findByRole('combobox', { name: /primary visual assistant/i }))
    expect(screen.queryByRole('option', { name: 'Text only' })).not.toBeInTheDocument()
    await user.click(screen.getByRole('option', { name: 'Vision primary' }))
    await waitFor(() =>
      expect(setVisualAssistance).toHaveBeenLastCalledWith({
        enabled: true,
        assistant_model: 'demo/vision-primary',
        fallback_models: [],
      }),
    )

    const fallback = screen.getByRole('combobox', { name: /visual fallback model/i })
    await user.click(fallback)
    await user.click(screen.getByRole('option', { name: 'Vision fallback A' }))
    await user.click(screen.getByRole('button', { name: /add fallback/i }))
    await user.click(fallback)
    await user.click(screen.getByRole('option', { name: 'Vision fallback B' }))
    await user.click(screen.getByRole('button', { name: /add fallback/i }))
    await user.click(screen.getByRole('button', { name: /move vision fallback b up/i }))
    await user.click(screen.getByRole('button', { name: /remove vision fallback a/i }))

    await waitFor(() =>
      expect(setVisualAssistance).toHaveBeenLastCalledWith({
        enabled: true,
        assistant_model: 'demo/vision-primary',
        fallback_models: ['demo/vision-fallback-b'],
      }),
    )
  })

  it('rejects enabling assistance with a text-only saved assistant', async () => {
    visualAssistance = { enabled: false, assistant_model: 'demo/text-only', fallback_models: [] }
    const user = userEvent.setup()
    render(<CodexPage />)

    await user.click(await screen.findByRole('switch', { name: /visual assistance/i }))

    expect(await screen.findByText(/does not support visual assistance/i)).toBeInTheDocument()
    expect(setVisualAssistance).not.toHaveBeenCalled()
  })

  it('auto-selects the first eligible vision model as primary when enabling', async () => {
    visualAssistance = { enabled: false, assistant_model: null, fallback_models: [] }
    const user = userEvent.setup()
    render(<CodexPage />)

    await user.click(await screen.findByRole('switch', { name: /visual assistance/i }))

    await waitFor(() =>
      expect(setVisualAssistance).toHaveBeenLastCalledWith({
        enabled: true,
        assistant_model: 'demo/vision-primary',
        fallback_models: [],
      }),
    )
  })

  it('offers a vision model that is not enabled for normal provider routing', async () => {
    visualAssistance = { enabled: false, assistant_model: null, fallback_models: [] }
    const user = userEvent.setup()
    render(<CodexPage />)

    await user.click(await screen.findByRole('combobox', { name: /primary visual assistant/i }))
    await user.click(screen.getByRole('option', { name: 'Vision catalog only' }))

    await waitFor(() =>
      expect(setVisualAssistance).toHaveBeenLastCalledWith({
        enabled: false,
        assistant_model: 'demo/vision-catalog-only',
        fallback_models: [],
      }),
    )
  })

  it('does not offer visual models from a provider without an API key', async () => {
    demoProviderHasKey = false
    visualAssistance = { enabled: true, assistant_model: null, fallback_models: [] }
    const user = userEvent.setup()
    render(<CodexPage />)

    await user.click(await screen.findByRole('combobox', { name: /primary visual assistant/i }))

    expect(screen.queryByRole('option', { name: 'Vision primary' })).not.toBeInTheDocument()
    expect(screen.queryByRole('option', { name: 'Vision fallback A' })).not.toBeInTheDocument()
    expect(screen.queryByRole('option', { name: 'Vision fallback B' })).not.toBeInTheDocument()
  })

  it('does not offer vision models served through the Responses protocol', async () => {
    visualAssistance = { enabled: true, assistant_model: null, fallback_models: [] }
    const user = userEvent.setup()
    render(<CodexPage />)

    await user.click(await screen.findByRole('combobox', { name: /primary visual assistant/i }))

    expect(screen.queryByRole('option', { name: 'Vision responses' })).not.toBeInTheDocument()
  })

  it('clears a pending fallback and removes a fallback promoted to primary', async () => {
    visualAssistance = {
      enabled: true,
      assistant_model: 'demo/vision-primary',
      fallback_models: [],
    }
    const user = userEvent.setup()
    render(<CodexPage />)

    const fallback = await screen.findByRole('combobox', { name: /visual fallback model/i })
    const addFallback = screen.getByRole('button', { name: /add fallback/i })
    await user.click(fallback)
    await user.click(screen.getByRole('option', { name: 'Vision fallback A' }))
    expect(addFallback).not.toBeDisabled()

    const primary = screen.getByRole('combobox', { name: /primary visual assistant/i })
    await user.click(primary)
    await user.click(screen.getByRole('option', { name: 'Vision fallback A' }))
    await waitFor(() => expect(addFallback).toBeDisabled())

    await user.click(fallback)
    expect(screen.queryByRole('option', { name: 'Vision fallback A' })).not.toBeInTheDocument()
    await user.click(screen.getByRole('option', { name: 'Vision fallback B' }))
    await user.click(addFallback)
    await waitFor(() =>
      expect(setVisualAssistance).toHaveBeenLastCalledWith({
        enabled: true,
        assistant_model: 'demo/vision-fallback-a',
        fallback_models: ['demo/vision-fallback-b'],
      }),
    )

    await user.click(primary)
    await user.click(screen.getByRole('option', { name: 'Vision fallback B' }))
    await waitFor(() =>
      expect(setVisualAssistance).toHaveBeenLastCalledWith({
        enabled: true,
        assistant_model: 'demo/vision-fallback-b',
        fallback_models: [],
      }),
    )
  })
})

describe('orphaned managed block', () => {
  it('warns when the config lost its markers to an external rewrite', async () => {
    orphaned = true
    render(<CodexPage />)
    expect(await screen.findByText(/rewritten externally/i)).toBeInTheDocument()
  })
})
