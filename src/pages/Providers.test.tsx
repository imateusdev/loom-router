import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'

const apiMocks = vi.hoisted(() => ({
  getConfig: vi.fn(async () => ({
    port: 4180,
    providers: {},
    side_call_fallback: null,
    native_slug_mode: false,
    onboarding_completed: true,
  })),
  validateProvider: vi.fn(async () => ['claude-opus-5', 'claude-sonnet-4-6']),
  saveProvider: vi.fn(async () => {}),
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
