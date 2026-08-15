// The first-run gate. Getting this wrong is invisible in development and
// very visible to a new user: either the walkthrough never runs, or it runs
// again for someone who has used the app for months.

import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { AppConfig, CodexStatus } from '@/types'

// A plain mutable implementation rather than a vi.fn: `restoreMocks` and
// per-test resets interact badly with a promise the component chains onto
// after the test body ends, and the runner reports it as unhandled.
let getConfig: () => Promise<AppConfig> = () => Promise.reject(new Error('unset'))
const statusPayload = (managed_block_orphaned: boolean): CodexStatus => ({
  codex_home: '~/.codex',
  config_exists: true,
  config_parseable: true,
  managed_block_present: false,
  managed_block_orphaned,
  native_catalog_present: false,
  merged_catalog_present: false,
  merged_model_count: 0,
  codex_cli_available: true,
  integration_enabled: false,
  session: {
    path: '~/.codex/auth.json',
    present: false,
    usable: false,
    has_account_id: false,
    expired: false,
    expires_in_hours: null,
    age_hours: null,
  },
})
let codexStatus: () => Promise<CodexStatus> = () => Promise.resolve(statusPayload(false))
let codexApply = vi.fn(() => Promise.resolve())

vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    getConfig: () => getConfig(),
    // Everything the pages call on mount; the gate is what is under test.
    serverStatus: () => Promise.resolve({ running: true, port: 4180, url: null }),
    providerBalances: () => Promise.resolve([]),
    contextWindows: () => Promise.resolve({}),
    statsSummary: () => Promise.resolve(null),
    codexStatus: () => codexStatus(),
    codexApply: () => codexApply(),
    detectTools: () =>
      Promise.resolve({
        claude: { detected: false, logged_in: null, already_imported: false },
        opencode: { config_found: false, gateways: [] },
      }),
    setupStatus: () =>
      Promise.resolve({
        ready: false,
        missing: ['codex_integration', 'provider'],
        validation: { started_at: null, first_ok_request_at: null, failed_attempt: false },
        codex_active: false,
      }),
    multiAgentStatus: () => Promise.resolve(false),
    completeOnboarding: () => Promise.resolve(),
  },
}))

import App from './App'

const config = (over: Partial<AppConfig> = {}) =>
  ({ port: 4180, providers: {}, side_call_fallback: null, native_slug_mode: false, ...over }) as AppConfig

beforeEach(() => {
  codexApply = vi.fn(() => Promise.resolve())
  codexStatus = () => Promise.resolve(statusPayload(false))
})

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
    // walkthrough - an upgrade must land straight in the app.
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

describe('codex repair prompt', () => {
  it('offers repair and re-applies an orphaned managed block', async () => {
    const user = userEvent.setup()
    getConfig = () => Promise.resolve(config({ onboarding_completed: true }))
    codexStatus = () => Promise.resolve(statusPayload(true))
    codexApply = vi.fn(() => {
      codexStatus = () => Promise.resolve(statusPayload(false))
      return Promise.resolve()
    })

    renderApp()
    expect(await screen.findByRole('heading', { name: /repair codex integration/i })).toBeInTheDocument()
    await user.click(screen.getByRole('button', { name: /^repair$/i }))
    expect(codexApply).toHaveBeenCalledTimes(1)
    await waitFor(() =>
      expect(screen.queryByRole('heading', { name: /repair codex integration/i })).not.toBeInTheDocument(),
    )
  })

  it('can be dismissed without applying', async () => {
    const user = userEvent.setup()
    getConfig = () => Promise.resolve(config({ onboarding_completed: true }))
    codexStatus = () => Promise.resolve(statusPayload(true))

    renderApp()
    await user.click(await screen.findByRole('button', { name: /ignore/i }))
    expect(screen.queryByRole('heading', { name: /repair codex integration/i })).not.toBeInTheDocument()
    expect(codexApply).not.toHaveBeenCalled()
  })
})
