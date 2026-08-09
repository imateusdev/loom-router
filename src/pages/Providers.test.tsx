import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'

const saveProvider = vi.fn()
const discoverModels = vi.fn()

vi.mock('@/lib/events', () => ({ useBackendState: () => {} }))

vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    getConfig: () =>
      Promise.resolve({
        port: 4180,
        providers: {},
        side_call_fallback: null,
        native_slug_mode: false,
        onboarding_completed: true,
      }),
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
    validateProvider: () => Promise.resolve(['claude-opus-5', 'claude-sonnet-4-6']),
    saveProvider: (provider: unknown) => {
      saveProvider(provider)
      return Promise.resolve()
    },
    discoverModels: (providerId: string) => {
      discoverModels(providerId)
      return Promise.resolve(['claude-opus-5', 'claude-sonnet-4-6'])
    },
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
      expect(saveProvider).toHaveBeenCalled()
      expect(discoverModels).toHaveBeenCalledWith('claude-code')
    })
  })
})
