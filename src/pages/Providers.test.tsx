import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import type { Provider } from '@/types'

const apiMocks = vi.hoisted(() => ({
  getConfig: vi.fn(async () => ({
    port: 4180,
    providers: {},
    side_call_fallback: null,
    native_slug_mode: false,
    onboarding_completed: true,
  })),
  validateProvider: vi.fn(async () => ['claude-opus-5', 'claude-sonnet-4-6']),
  saveProvider: vi.fn(async (_provider: Provider) => {
    void _provider
  }),
  discoverModels: vi.fn(async () => ['claude-opus-5', 'claude-sonnet-4-6']),
}))

vi.mock('@/lib/events', () => ({ useBackendState: () => {} }))

vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    getConfig: apiMocks.getConfig,
    contextWindows: () => Promise.resolve({}),
    claudeAuthStatus: () =>
      Promise.resolve({
        logged_in: true,
        auth_method: 'claude.ai',
        subscription_type: 'max',
        email: 'test@example.com',
        plan: 'Max',
        error: null,
      }),
    validateProvider: apiMocks.validateProvider,
    saveProvider: apiMocks.saveProvider,
    discoverModels: apiMocks.discoverModels,
    toggleModel: () => Promise.resolve(),
    setProviderEnabled: () => Promise.resolve(),
    setProviderRotation: () => Promise.resolve(),
  },
}))

import ProvidersPage from './Providers'

describe('Provider add flow', () => {
  it('auto-fetches models right after saving a new provider', async () => {
    const user = userEvent.setup()
    render(<ProvidersPage />)

    await user.click(await screen.findByRole('button', { name: /add provider/i }))
    await user.click(screen.getByRole('button', { name: /^save$/i }))

    await waitFor(() => {
      expect(apiMocks.saveProvider).toHaveBeenCalled()
      expect(apiMocks.discoverModels).toHaveBeenCalledWith('claude-code')
    })
  })

  it('keeps the add dialog open when save anyway fails and never calls discovery or onSaved', async () => {
    apiMocks.saveProvider.mockClear()
    apiMocks.discoverModels.mockClear()
    apiMocks.getConfig.mockClear()
    apiMocks.validateProvider.mockClear()

    const user = userEvent.setup()
    render(<ProvidersPage />)

    await user.click(await screen.findByRole('button', { name: /add provider/i }))

    apiMocks.validateProvider.mockRejectedValueOnce(new Error('bad key'))
    await user.click(screen.getByRole('button', { name: /^save$/i }))

    expect(await screen.findByText(/bad key/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /save anyway/i })).toBeInTheDocument()

    const getConfigCalls = apiMocks.getConfig.mock.calls.length
    apiMocks.saveProvider.mockRejectedValueOnce(new Error('disk full'))
    await user.click(screen.getByRole('button', { name: /save anyway/i }))

    expect(await screen.findByText(/disk full/)).toBeInTheDocument()
    expect(apiMocks.saveProvider).toHaveBeenCalledTimes(1)
    expect(apiMocks.discoverModels).not.toHaveBeenCalled()
    expect(apiMocks.getConfig.mock.calls.length).toBe(getConfigCalls)
    expect(screen.getByRole('button', { name: /save anyway/i })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /^save$/i })).toBeInTheDocument()
  })
})

const keyedProvider = (over: Partial<Provider> = {}): Provider => ({
  id: 'acme',
  name: 'Acme',
  protocol: 'openai',
  base_url: 'https://api.acme.test/v1',
  api_key: null,
  keys: [
    { id: 'key-a', name: 'Alpha', enabled: true, api_key: null, has_key: true },
    { id: 'key-b', name: 'Beta', enabled: true, api_key: null, has_key: true },
  ],
  rotation_enabled: false,
  has_key: true,
  user_agent: null,
  models: [],
  enabled: true,
  ...over,
})

const renderKeyedProvider = async (provider: Provider) => {
  apiMocks.getConfig.mockResolvedValue({
    port: 4180,
    providers: { [provider.id]: provider },
    side_call_fallback: null,
    native_slug_mode: false,
    onboarding_completed: true,
  })
  render(<ProvidersPage />)
  return await screen.findByText(provider.name)
}

describe('provider key management', () => {
  it('UT-100 UT-101 E2E-001 adds a named key and keeps the value hidden', async () => {
    const provider = keyedProvider()
    let saved = provider
    apiMocks.saveProvider.mockImplementation(async (next: Provider) => {
      saved = next
      apiMocks.getConfig.mockResolvedValue({
        port: 4180,
        providers: { [saved.id]: saved },
        side_call_fallback: null,
        native_slug_mode: false,
        onboarding_completed: true,
      })
    })
    const user = userEvent.setup()
    await renderKeyedProvider(provider)

    await user.click(await screen.findByRole('button', { name: /add key/i }))
    await user.type(screen.getByPlaceholderText(/key name/i), 'Gamma')
    await user.type(screen.getByPlaceholderText(/API key/i), 'sk-gamma')
    await user.click(screen.getByRole('button', { name: /^save$/i }))

    expect(await screen.findByText('Gamma')).toBeInTheDocument()
    expect(apiMocks.saveProvider).toHaveBeenCalled()
    expect(saved.keys.at(-1)).toMatchObject({ name: 'Gamma', has_key: true })
  })

  it('UT-102 UT-103 blocks empty and duplicate key names', async () => {
    const provider = keyedProvider()
    const user = userEvent.setup()
    await renderKeyedProvider(provider)

    await user.click(await screen.findByRole('button', { name: /add key/i }))
    await user.click(screen.getByRole('button', { name: /^save$/i }))
    expect((await screen.findAllByText(/enter a key name/i)).length).toBeGreaterThan(0)

    await user.type(screen.getByPlaceholderText(/key name/i), 'Alpha')
    await user.type(screen.getByPlaceholderText(/API key/i), 'sk-gamma')
    await user.click(screen.getByRole('button', { name: /^save$/i }))
    expect(await screen.findByText(/already exists/i)).toBeInTheDocument()
  })

  it('UT-107 UT-108 UT-109 renames and validates rename', async () => {
    const provider = keyedProvider()
    const user = userEvent.setup()
    await renderKeyedProvider(provider)

    await user.click(await screen.findByRole('button', { name: 'Rename key Alpha' }))
    const nameInput = screen.getByPlaceholderText(/key name/i)
    await user.clear(nameInput)
    await user.click(screen.getByRole('button', { name: /^save$/i }))
    expect(await screen.findByText(/enter a key name/i)).toBeInTheDocument()

    await user.type(nameInput, 'Beta')
    await user.click(screen.getByRole('button', { name: /^save$/i }))
    expect(await screen.findByText(/already exists/i)).toBeInTheDocument()

    await user.clear(nameInput)
    await user.type(nameInput, 'Primary')
    await user.click(screen.getByRole('button', { name: /^save$/i }))
    expect((await screen.findAllByText('Primary')).length).toBeGreaterThan(0)
  })

  it('UT-112 UT-113 UT-116 warns on last key and supports cancel', async () => {
    const provider = keyedProvider({ keys: [{ id: 'key-a', name: 'Only', enabled: true, api_key: null, has_key: true }] })
    const user = userEvent.setup()
    await renderKeyedProvider(provider)

    await user.click(await screen.findByRole('button', { name: 'Delete key Only' }))
    expect(await screen.findByText(/no stored credential/i)).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: /cancel/i }))
    expect(screen.getByText('Only')).toBeInTheDocument()
  })

  it('UT-117 UT-119 reorders keys with stable controls', async () => {
    const provider = keyedProvider()
    let saved = provider
    apiMocks.saveProvider.mockImplementation(async (next: Provider) => {
      saved = next
      apiMocks.getConfig.mockResolvedValue({
        port: 4180,
        providers: { [saved.id]: saved },
        side_call_fallback: null,
        native_slug_mode: false,
        onboarding_completed: true,
      })
    })
    const user = userEvent.setup()
    await renderKeyedProvider(provider)

    const up = await screen.findByRole('button', { name: 'Move up Beta' })
    expect(up).toBeEnabled()
    expect(screen.getByRole('button', { name: 'Move up Alpha' })).toBeDisabled()
    await user.click(up)

    expect(await screen.findByText('Beta')).toBeInTheDocument()
    expect(saved.keys.map((key) => key.id)).toEqual(['key-b', 'key-a'])
  })

  it('UT-118 UT-120 disabled key at top is not primary', async () => {
    const provider = keyedProvider({
      keys: [
        { id: 'key-a', name: 'Alpha', enabled: false, api_key: null, has_key: true },
        { id: 'key-b', name: 'Beta', enabled: true, api_key: null, has_key: true },
      ],
    })
    await renderKeyedProvider(provider)

    expect(await screen.findByText('Primary')).toBeInTheDocument()
    expect(screen.getByText('Primary').closest('div')?.textContent).toContain('Beta')
  })

  it('UT-123 UT-124 UT-125 UT-126 disables, shows all-disabled and re-enables', async () => {
    const provider = keyedProvider({
      keys: [{ id: 'key-a', name: 'Only', enabled: true, api_key: null, has_key: true }],
    })
    let saved = provider
    apiMocks.saveProvider.mockImplementation(async (next: Provider) => {
      saved = next
      apiMocks.getConfig.mockResolvedValue({
        port: 4180,
        providers: { [saved.id]: saved },
        side_call_fallback: null,
        native_slug_mode: false,
        onboarding_completed: true,
      })
    })
    const user = userEvent.setup()
    await renderKeyedProvider(provider)

    await user.click(await screen.findByRole('switch', { name: 'Enable key Only' }))
    expect(await screen.findByText('All keys disabled')).toBeInTheDocument()
    expect(saved.keys[0].enabled).toBe(false)

    await user.click(screen.getByRole('switch', { name: 'Enable key Only' }))
    expect(await screen.findByText('Only')).toBeInTheDocument()
    expect(saved.keys[0].enabled).toBe(true)
  })

  it('editing the name keeps the stored keys and rotation', async () => {
    // Editing sent `keys: []` and `rotation_enabled: false`, and the backend
    // replaces the provider wholesale - a rename wiped every stored key.
    const provider = keyedProvider({ rotation_enabled: true })
    apiMocks.saveProvider.mockImplementation(async () => {})
    const user = userEvent.setup()
    await renderKeyedProvider(provider)

    await user.click(await screen.findByRole('button', { name: /more actions/i }))
    await user.click(screen.getByRole('button', { name: /^edit$/i }))
    await user.type(screen.getByPlaceholderText(/^name$/i), ' Renamed')
    await user.click(screen.getByRole('button', { name: /^save$/i }))

    await waitFor(() => expect(apiMocks.saveProvider).toHaveBeenCalled())
    const sent = apiMocks.saveProvider.mock.calls.at(-1)![0]
    expect(sent.name).toBe('Acme Renamed')
    expect(sent.keys.map((key) => key.id)).toEqual(['key-a', 'key-b'])
    expect(sent.rotation_enabled).toBe(true)
  })

  it('UT-129 shows rotation only when keys exist', async () => {
    const provider = keyedProvider()
    await renderKeyedProvider(provider)

    expect(await screen.findByRole('switch', { name: /rotate requests across keys/i })).toBeInTheDocument()
  })
})

describe('provider tabs', () => {
  it('shows API keys by default and switches to models', async () => {
    const provider = keyedProvider({
      models: [{ id: 'opus', enabled: true, supports_vision: true }],
    })
    const user = userEvent.setup()
    await renderKeyedProvider(provider)

    expect(screen.getByRole('button', { name: /add key/i })).toBeInTheDocument()
    expect(screen.queryByText('opus')).not.toBeInTheDocument()

    await user.click(screen.getByRole('tab', { name: 'Models' }))

    expect(await screen.findByText('opus')).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /add key/i })).not.toBeInTheDocument()
  })
})
