// Overview setup banner contract: UT-081..084 and E2E-003 from _tests.md.

import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter, Route, Routes } from 'react-router'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { SetupStatus } from '@/types'

let setupStatus: SetupStatus = {
  ready: true,
  missing: [],
  validation: { started_at: null, first_ok_request_at: null, failed_attempt: false },
  codex_active: true,
}

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
    providerBalances: () => Promise.resolve([]),
    contextWindows: () => Promise.resolve({}),
    statsSummary: () =>
      Promise.resolve({
        period_secs: 86_400,
        requests: 0,
        input_tokens: 0,
        output_tokens: 0,
        cached_tokens: 0,
        cache_ratio: 0,
        cost_usd: 0,
        per_provider: [],
      }),
    setupStatus: () => Promise.resolve(setupStatus),
  },
}))

import OverviewPage from './Overview'

const renderOverview = () =>
  render(
    <MemoryRouter initialEntries={['/']}>
      <Routes>
        <Route path="/" element={<OverviewPage />} />
        <Route path="/codex" element={<div>Codex page</div>} />
        <Route path="/providers" element={<div>Providers page</div>} />
      </Routes>
    </MemoryRouter>,
  )

beforeEach(() => {
  setupStatus = {
    ready: true,
    missing: [],
    validation: { started_at: null, first_ok_request_at: null, failed_attempt: false },
    codex_active: true,
  }
  window.sessionStorage.clear()
})

describe('Overview setup banner', () => {
  it('UT-081 renders no banner when setup is ready', async () => {
    renderOverview()
    await waitFor(() => expect(screen.getByRole('heading', { name: /overview/i })).toBeInTheDocument())
    expect(screen.queryByText(/setup is not complete/i)).not.toBeInTheDocument()
  })

  it('UT-082 names Codex integration and links to /codex', async () => {
    setupStatus = {
      ready: false,
      missing: ['codex_integration'],
      validation: { started_at: null, first_ok_request_at: null, failed_attempt: false },
      codex_active: false,
    }
    renderOverview()
    expect(await screen.findByText(/setup is not complete/i)).toBeInTheDocument()
    const link = screen.getByRole('link', { name: /connect codex integration/i })
    expect(link).toHaveAttribute('href', '/codex')
  })

  it('UT-083 names provider/model and links to /providers', async () => {
    setupStatus = {
      ready: false,
      missing: ['provider', 'enabled_model'],
      validation: { started_at: null, first_ok_request_at: null, failed_attempt: false },
      codex_active: true,
    }
    renderOverview()
    expect(await screen.findByText(/setup is not complete/i)).toBeInTheDocument()
    expect(screen.getByRole('link', { name: /add a provider/i })).toHaveAttribute('href', '/providers')
    expect(screen.getByRole('link', { name: /enable a model/i })).toHaveAttribute('href', '/providers')
  })

  it('UT-084 dismissed banner returns on the next app launch while incomplete', async () => {
    setupStatus = {
      ready: false,
      missing: ['codex_integration'],
      validation: { started_at: null, first_ok_request_at: null, failed_attempt: false },
      codex_active: false,
    }
    const first = renderOverview()
    await screen.findByText(/setup is not complete/i)
    await userEvent.click(screen.getByRole('button', { name: /dismiss setup reminder/i }))
    expect(screen.queryByText(/setup is not complete/i)).not.toBeInTheDocument()

    first.unmount()
    window.sessionStorage.clear()
    renderOverview()
    expect(await screen.findByText(/setup is not complete/i)).toBeInTheDocument()
  })

  it('E2E-003 links navigate and the banner disappears once ready', async () => {
    setupStatus = {
      ready: false,
      missing: ['codex_integration'],
      validation: { started_at: null, first_ok_request_at: null, failed_attempt: false },
      codex_active: false,
    }
    const user = userEvent.setup()
    const journey = renderOverview()
    await user.click(await screen.findByRole('link', { name: /connect codex integration/i }))
    expect(await screen.findByText('Codex page')).toBeInTheDocument()
    journey.unmount()

    setupStatus = {
      ready: true,
      missing: [],
      validation: { started_at: null, first_ok_request_at: null, failed_attempt: false },
      codex_active: true,
    }
    const ready = render(
      <MemoryRouter initialEntries={['/']}>
        <Routes>
          <Route path="/" element={<OverviewPage />} />
        </Routes>
      </MemoryRouter>,
    )
    expect(await ready.findByRole('heading', { name: /overview/i })).toBeInTheDocument()
    expect(screen.queryByText(/setup is not complete/i)).not.toBeInTheDocument()
  })
})
