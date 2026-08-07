import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'

const setModelVision = vi.fn((...args: [string, string, boolean]) => {
  void args
  return Promise.resolve()
})

vi.mock('@/lib/events', () => ({ useBackendState: () => {} }))

vi.mock('@/lib/api', () => ({
  api: {
    contextWindows: () => Promise.resolve({}),
    getConfig: () =>
      Promise.resolve({
        port: 4180,
        side_call_fallback: null,
        visual_assistance: { enabled: false, assistant_model: null, fallback_models: [] },
        native_slug_mode: false,
        providers: {
          demo: {
            id: 'demo',
            name: 'Demo',
            protocol: 'openai',
            base_url: 'https://example.test',
            has_key: true,
            enabled: true,
            models: [
              { id: 'vision-model', label: 'Vision model', enabled: true, supports_vision: false },
            ],
          },
        },
      }),
    setModelVision: (providerId: string, model: string, supports: boolean) =>
      setModelVision(providerId, model, supports),
  },
}))

import ProvidersPage from './Providers'

describe('model vision capability', () => {
  it('persists a model vision capability toggle', async () => {
    const user = userEvent.setup()
    render(<ProvidersPage />)

    const toggles = await screen.findAllByRole('switch', { name: /vision support for vision model/i })
    const toggle = toggles.at(-1) as HTMLElement
    expect(toggle).not.toBeChecked()
    await user.click(toggle)

    await waitFor(() => expect(setModelVision).toHaveBeenCalledWith('demo', 'vision-model', true))
  })
})
