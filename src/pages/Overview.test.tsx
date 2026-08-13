// Overview setup banner contract: UT-081..084 and E2E-003 from _tests.md.

import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter, Route, Routes } from 'react-router'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ProviderBalance, SetupStatus, StatsSummary } from '@/types'

let setupStatus: SetupStatus = {
  ready: true,
  missing: [],
  validation: { started_at: null, first_ok_request_at: null, failed_attempt: false },
  codex_active: true,
}

let mockConfig = {
  port: 4180,
  providers: {},
  side_call_fallback: null,
  native_slug_mode: false,
  onboarding_completed: true,
}
let mockBalances: ProviderBalance[] = []
let mockStats: StatsSummary = {
  period_secs: 86_400,
  requests: 0,
  input_tokens: 0,
  output_tokens: 0,
  cached_tokens: 0,
  cache_ratio: 0,
  cost_usd: 0,
  per_provider: [],
  per_key: [],
}

vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    getConfig: () =>
      Promise.resolve(mockConfig),
    providerBalances: () => Promise.resolve(mockBalances),
    contextWindows: () => Promise.resolve({}),
    statsSummary: () => Promise.resolve(mockStats),
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
  mockConfig = {
    port: 4180,
    providers: {},
    side_call_fallback: null,
    native_slug_mode: false,
    onboarding_completed: true,
  }
  mockBalances = []
  mockStats = {
    period_secs: 86_400,
    requests: 0,
    input_tokens: 0,
    output_tokens: 0,
    cached_tokens: 0,
    cache_ratio: 0,
    cost_usd: 0,
    per_provider: [],
    per_key: [],
  }
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

describe('Overview per-key dashboard', () => {
  it('E2E-004 shows per-key usage and not-reported states', async () => {
    mockConfig = {
      port: 4180,
      providers: {
        acme: {
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
        },
      },
      side_call_fallback: null,
      native_slug_mode: false,
      onboarding_completed: true,
    }
    mockBalances = [
      {
        provider_id: 'acme',
        key_id: 'key-a',
        key_name: 'Alpha',
        ok: true,
        bars: [],
        balance_text: null,
        error: null,
      },
      {
        provider_id: 'acme',
        key_id: 'key-b',
        key_name: 'Beta',
        ok: true,
        bars: [],
        balance_text: null,
        error: null,
      },
    ]
    mockStats = {
      period_secs: 86_400,
      requests: 7,
      input_tokens: 100,
      output_tokens: 20,
      cached_tokens: 10,
      cache_ratio: 0.1,
      cost_usd: 0,
      per_provider: [],
      per_key: [
        { key_id: 'key-a', key_name: 'Alpha', requests: 5, errors: 0, input_tokens: 100, output_tokens: 20, cached_tokens: 10 },
        { key_id: 'key-b', key_name: 'Beta', requests: 2, errors: 0, input_tokens: 0, output_tokens: 0, cached_tokens: 0 },
      ],
    }

    renderOverview()

    expect(await screen.findByText('Alpha')).toBeInTheDocument()
    expect(screen.getByText('Beta')).toBeInTheDocument()
    expect(screen.getByText('5')).toBeInTheDocument()
    expect(screen.getByText('2')).toBeInTheDocument()
    expect(screen.getAllByText(/not reported/i).length).toBeGreaterThan(0)
  })

  it('formats quota reset_at with the active locale', async () => {
    mockBalances = [
      {
        provider_id: 'acme',
        key_id: 'key-a',
        key_name: 'Alpha',
        ok: true,
        bars: [
          {
            label: 'Weekly quota',
            percent: 67,
            detail: '67 / 100 left',
            reset_at: '2026-08-13T02:33:12Z',
          },
        ],
        balance_text: null,
        error: null,
      },
    ]

    renderOverview()

    const resetAt = new Intl.DateTimeFormat('en', {
      month: 'short',
      day: 'numeric',
      hour: '2-digit',
      minute: '2-digit',
    }).format(new Date('2026-08-13T02:33:12Z'))
    expect(await screen.findByText(`67 / 100 left · resets ${resetAt}`)).toBeInTheDocument()
  })

  it('E2E-002 shows the primary key attribution after routing', async () => {
    mockConfig = {
      port: 4180,
      providers: {
        acme: {
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
        },
      },
      side_call_fallback: null,
      native_slug_mode: false,
      onboarding_completed: true,
    }
    mockBalances = [
      { provider_id: 'acme', key_id: 'key-a', key_name: 'Alpha', ok: true, bars: [], balance_text: null, error: null },
      { provider_id: 'acme', key_id: 'key-b', key_name: 'Beta', ok: true, bars: [], balance_text: null, error: null },
    ]
    mockStats = {
      period_secs: 86_400,
      requests: 3,
      input_tokens: 30,
      output_tokens: 10,
      cached_tokens: 0,
      cache_ratio: 0,
      cost_usd: 0,
      per_provider: [],
      per_key: [{ key_id: 'key-b', key_name: 'Beta', requests: 3, errors: 0, input_tokens: 30, output_tokens: 10, cached_tokens: 0 }],
    }

    renderOverview()

    expect((await screen.findAllByText('3')).length).toBeGreaterThan(0)
    expect(screen.getByText('Beta')).toBeInTheDocument()
  })

  it('E2E-003 shows rotation distributed across named keys', async () => {
    mockConfig = {
      port: 4180,
      providers: {
        acme: {
          id: 'acme',
          name: 'Acme',
          protocol: 'openai',
          base_url: 'https://api.acme.test/v1',
          api_key: null,
          keys: [
            { id: 'key-a', name: 'Alpha', enabled: true, api_key: null, has_key: true },
            { id: 'key-b', name: 'Beta', enabled: true, api_key: null, has_key: true },
          ],
          rotation_enabled: true,
          has_key: true,
          user_agent: null,
          models: [],
          enabled: true,
        },
      },
      side_call_fallback: null,
      native_slug_mode: false,
      onboarding_completed: true,
    }
    mockBalances = [
      { provider_id: 'acme', key_id: 'key-a', key_name: 'Alpha', ok: true, bars: [], balance_text: null, error: null },
      { provider_id: 'acme', key_id: 'key-b', key_name: 'Beta', ok: true, bars: [], balance_text: null, error: null },
    ]
    mockStats = {
      period_secs: 86_400,
      requests: 10,
      input_tokens: 50,
      output_tokens: 20,
      cached_tokens: 0,
      cache_ratio: 0,
      cost_usd: 0,
      per_provider: [],
      per_key: [
        { key_id: 'key-a', key_name: 'Alpha', requests: 4, errors: 0, input_tokens: 20, output_tokens: 8, cached_tokens: 0 },
        { key_id: 'key-b', key_name: 'Beta', requests: 6, errors: 0, input_tokens: 30, output_tokens: 12, cached_tokens: 0 },
      ],
    }

    renderOverview()

    expect(await screen.findByText('4')).toBeInTheDocument()
    expect(screen.getByText('6')).toBeInTheDocument()
  })

  it('E2E-006 shows an all-keys failure without key values', async () => {
    mockConfig = {
      port: 4180,
      providers: {
        acme: {
          id: 'acme',
          name: 'Acme',
          protocol: 'openai',
          base_url: 'https://api.acme.test/v1',
          api_key: null,
          keys: [
            { id: 'key-a', name: 'Alpha', enabled: true, api_key: null, has_key: true },
          ],
          rotation_enabled: false,
          has_key: true,
          user_agent: null,
          models: [],
          enabled: true,
        },
      },
      side_call_fallback: null,
      native_slug_mode: false,
      onboarding_completed: true,
    }
    mockBalances = [
      {
        provider_id: 'acme',
        key_id: 'key-a',
        key_name: 'Alpha',
        ok: false,
        bars: [],
        balance_text: null,
        error: 'provider rejected all configured credentials',
      },
    ]
    mockStats = {
      period_secs: 86_400,
      requests: 0,
      input_tokens: 0,
      output_tokens: 0,
      cached_tokens: 0,
      cache_ratio: 0,
      cost_usd: 0,
      per_provider: [],
      per_key: [],
    }

    renderOverview()

    expect(await screen.findByText(/provider rejected all configured credentials/i)).toBeInTheDocument()
    expect(screen.queryByText(/sk-secret/i)).not.toBeInTheDocument()
  })
})

describe('Overview Analytics tab', () => {
  it('shows the empty state when stats has no plottable models', async () => {
    renderOverview()
    await screen.findByText(/no requests in this period/i)
    await userEvent.click(screen.getByRole('tab', { name: 'Analytics' }))

    expect(await screen.findByText(/no plottable usage yet/i)).toBeInTheDocument()
  })

  it('plots average cost per request instead of aggregate cost', async () => {
    mockStats = {
      period_secs: 86_400,
      requests: 10,
      input_tokens: 1000,
      output_tokens: 100,
      cached_tokens: 0,
      cache_ratio: 0,
      cost_usd: 2,
      per_provider: [
        {
          provider: 'opencode-go',
          requests: 10,
          input_tokens: 1000,
          output_tokens: 100,
          cached_tokens: 0,
          cost_usd: 2,
          models: [
            {
              model: 'opencode-go/deepseek-v4-flash',
              requests: 10,
              errors: 0,
              input_tokens: 1000,
              output_tokens: 100,
              cached_tokens: 0,
              cache_ratio: 0,
              avg_latency_ms: 1000,
              cost_usd: 2,
            },
          ],
        },
      ],
      per_key: [],
    }
    const { container } = renderOverview()
    await screen.findByText('opencode-go/deepseek-v4-flash')
    await userEvent.click(screen.getByRole('tab', { name: 'Analytics' }))

    await screen.findByText('Avg cost / request (log)')
    const tooltip = container.querySelector('circle title')?.textContent ?? ''
    expect(tooltip).toContain('$0.2')
  })

  it('merges the same model served by multiple providers into one bubble', async () => {
    mockStats = {
      period_secs: 86_400,
      requests: 15,
      input_tokens: 0,
      output_tokens: 0,
      cached_tokens: 0,
      cache_ratio: 0,
      cost_usd: 3,
      per_provider: [
        {
          provider: 'opencode-go',
          requests: 10,
          input_tokens: 0,
          output_tokens: 0,
          cached_tokens: 0,
          cost_usd: 2,
          models: [
            {
              model: 'opencode-go/deepseek-v4-flash',
              requests: 10,
              errors: 0,
              input_tokens: 0,
              output_tokens: 0,
              cached_tokens: 0,
              cache_ratio: 0,
              avg_latency_ms: 1000,
              cost_usd: 2,
            },
          ],
        },
        {
          provider: 'opencode-zen',
          requests: 5,
          input_tokens: 0,
          output_tokens: 0,
          cached_tokens: 0,
          cost_usd: 1,
          models: [
            {
              model: 'opencode-zen/deepseek-v4-flash',
              requests: 5,
              errors: 0,
              input_tokens: 0,
              output_tokens: 0,
              cached_tokens: 0,
              cache_ratio: 0,
              avg_latency_ms: 500,
              cost_usd: 1,
            },
          ],
        },
      ],
      per_key: [],
    }
    const { container } = renderOverview()
    await userEvent.click(screen.getByRole('tab', { name: 'Analytics' }))
    await screen.findByText('Avg cost / request (log)')

    expect(container.querySelectorAll('[data-testid="marker-deepseek-v4-flash"]')).toHaveLength(1)
    const tooltip = container.querySelector('circle title')?.textContent ?? ''
    expect(tooltip).toContain('$0.2')
    expect(tooltip).toContain('15')
  })
})
