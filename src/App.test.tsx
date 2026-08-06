// The first-run gate. Getting this wrong is invisible in development and
// very visible to a new user: either the walkthrough never runs, or it runs
// again for someone who has used the app for months.

import { render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router'
import { describe, expect, it, vi } from 'vitest'
import type { AppConfig } from '@/types'

// A plain mutable implementation rather than a vi.fn: `restoreMocks` and
// per-test resets interact badly with a promise the component chains onto
// after the test body ends, and the runner reports it as unhandled.
let getConfig: () => Promise<AppConfig> = () => Promise.reject(new Error('unset'))

vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    getConfig: () => getConfig(),
    // Everything the pages call on mount; the gate is what is under test.
    serverStatus: () => Promise.resolve({ running: true, port: 4180, url: null }),
    providerBalances: () => Promise.resolve([]),
    contextWindows: () => Promise.resolve({}),
    statsSummary: () => Promise.resolve(null),
    codexStatus: () =>
      Promise.resolve({
        managed_block_present: false,
        managed_block_orphaned: false,
        codex_cli_available: true,
      }),
    multiAgentStatus: () => Promise.resolve(false),
    completeOnboarding: () => Promise.resolve(),
  },
}))

import App from './App'

const config = (over: Partial<AppConfig> = {}) =>
  ({ port: 4180, providers: {}, side_call_fallback: null, native_slug_mode: false, ...over }) as AppConfig

const renderApp = () => render(<MemoryRouter><App /></MemoryRouter>)

describe('first-run gate', () => {
  it('runs the walkthrough when the answer was never recorded', async () => {
    // A fresh install: the field is absent, not false.
    getConfig = () => Promise.resolve(config())
    renderApp()
    expect(await screen.findByRole('button', { name: /start/i })).toBeInTheDocument()
  })

  it('does not replay the walkthrough for an existing install', async () => {
    // The backend backfills `true` for any config that predates the
    // walkthrough — an upgrade must land straight in the app.
    getConfig = () => Promise.resolve(config({ onboarding_completed: true }))
    renderApp()
    expect(await screen.findByRole('heading', { name: /overview/i })).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /^start$/i })).not.toBeInTheDocument()
  })

  it('still runs the walkthrough when it was explicitly left unfinished', async () => {
    // `false` is what gets persisted mid-walkthrough; it must not be
    // mistaken for "done".
    getConfig = () => Promise.resolve(config({ onboarding_completed: false }))
    renderApp()
    expect(await screen.findByRole('button', { name: /start/i })).toBeInTheDocument()
  })

  it('falls through to the app when the config cannot be read', async () => {
    // A broken read must never trap someone in onboarding forever.
    getConfig = () => Promise.reject(new Error('config unreadable'))
    renderApp()
    expect(await screen.findByRole('heading', { name: /overview/i })).toBeInTheDocument()
    // The page mounts and reads the config again on its own. Let those
    // rejections settle inside the test, or the runner sees a handled
    // rejection that simply had not been drained yet and calls it unhandled.
    await new Promise((r) => setTimeout(r, 0))
  })

  it('renders nothing until the answer is known', async () => {
    // Rendering the app first and swapping a tick later flashes the wrong
    // screen on every launch.
    let resolve: (c: AppConfig) => void = () => {}
    const pending = new Promise<AppConfig>((r) => (resolve = r))
    getConfig = () => pending
    const { container } = renderApp()
    expect(container).toBeEmptyDOMElement()
    resolve(config({ onboarding_completed: true }))
    await waitFor(() => expect(container).not.toBeEmptyDOMElement())
  })
})
