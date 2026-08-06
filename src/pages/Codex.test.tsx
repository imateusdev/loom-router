// Regression cover for a one-way door: the Agents page showed a multi-agent
// prompt only while the feature was off, so enabling it removed the only
// control that existed. The backend always accepted `false`; nothing in the
// UI ever called it. The switch must be reachable in both states.

import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'

let multiAgent = false
const setMultiAgent = vi.fn((next: boolean) => {
  multiAgent = next
  return Promise.resolve(next)
})

vi.mock('@/lib/events', () => ({ useBackendState: () => {} }))

vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    codexStatus: () =>
      Promise.resolve({
        codex_home: '~/.codex',
        config_exists: true,
        managed_block_present: true,
        native_catalog_present: true,
        merged_catalog_present: true,
        merged_model_count: 3,
        codex_cli_available: true,
        integration_enabled: true,
      }),
    getConfig: () =>
      Promise.resolve({
        port: 4180,
        providers: {},
        side_call_fallback: null,
        native_slug_mode: false,
      }),
    multiAgentStatus: () => Promise.resolve(multiAgent),
    setMultiAgent: (v: boolean) => setMultiAgent(v),
    codexApply: () => Promise.resolve(),
    codexRemove: () => Promise.resolve(),
    setSideCallFallback: () => Promise.resolve(),
    setNativeSlugMode: () => Promise.resolve(),
  },
}))

import CodexPage from './Codex'

const multiAgentSwitch = async () => {
  const card = (await screen.findByText(/multi-agent/i)).closest('div[class*="rounded"]')
  return within(card as HTMLElement).getByRole('switch')
}

describe('multi-agent control', () => {
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
