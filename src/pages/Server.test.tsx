import { render, screen } from '@testing-library/react'
import { describe, expect, it, vi } from 'vitest'

vi.mock('@/lib/events', () => ({ useBackendState: () => {} }))

vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    serverStatus: () =>
      Promise.resolve({
        running: true,
        port: 4180,
        url: 'http://127.0.0.1:4180/v1',
      }),
    statsSummary: () =>
      Promise.resolve({
        period_secs: 86_400,
        requests: 379,
        input_tokens: 2_000_000,
        output_tokens: 888_600,
        cached_tokens: 8_600_000,
        cache_ratio: 0.81,
        cost_usd: 14.72,
        per_provider: [
          {
            provider: 'deepseek',
            requests: 379,
            input_tokens: 2_000_000,
            output_tokens: 888_600,
            cached_tokens: 8_600_000,
            cost_usd: 14.72,
            models: [
              {
                model: 'deepseek/deepseek-chat',
                requests: 379,
                errors: 3,
                input_tokens: 2_000_000,
                output_tokens: 888_600,
                cached_tokens: 8_600_000,
                cache_ratio: 0.81,
                avg_latency_ms: 2840,
                cost_usd: 14.72,
              },
            ],
          },
        ],
      }),
    getConfig: () =>
      Promise.resolve({
        port: 4180,
        providers: {
          deepseek: {
            id: 'deepseek',
            name: 'DeepSeek',
            protocol: 'openai',
            base_url: 'https://api.deepseek.com/v1',
            has_key: true,
            enabled: true,
            models: [{ id: 'deepseek-chat', enabled: true, supports_vision: false }],
          },
          'kimi-coding': {
            id: 'kimi-coding',
            name: 'Kimi Code',
            protocol: 'openai',
            base_url: 'https://api.kimi.com/coding/v1',
            has_key: true,
            enabled: true,
            models: [{ id: 'k3', enabled: true, supports_vision: false }],
          },
        },
        side_call_fallback: null,
        native_slug_mode: false,
        onboarding_completed: true,
      }),
    codexStatus: () =>
      Promise.resolve({
        codex_home: '~/.codex',
        config_exists: true,
        managed_block_present: true,
        managed_block_orphaned: false,
        native_catalog_present: true,
        merged_catalog_present: true,
        merged_model_count: 2,
        codex_cli_available: true,
        integration_enabled: true,
      }),
    serverStart: () => Promise.resolve(),
    serverStop: () => Promise.resolve(),
  },
}))

import ServerPage from './Server'

describe('Server page', () => {
  it('shows proxy connection, integration and traffic conclusions', async () => {
    render(<ServerPage />)

    expect(await screen.findByRole('heading', { name: /server/i })).toBeInTheDocument()
    expect(screen.getByText('Running')).toBeInTheDocument()
    expect(screen.getByText('http://127.0.0.1:4180/v1')).toBeInTheDocument()
    expect(screen.getByText('Active')).toBeInTheDocument()
    expect(screen.getByText('Providers enabled').nextElementSibling).toHaveTextContent('2')
    expect(screen.getByText('Models enabled').nextElementSibling).toHaveTextContent('2')
    expect(screen.getAllByText('379').length).toBeGreaterThan(0)
    expect(screen.getAllByText('$14.72').length).toBeGreaterThan(0)
    expect(screen.getAllByText('DeepSeek').length).toBeGreaterThan(0)
    expect(screen.getAllByText('Kimi Code').length).toBeGreaterThan(0)
    expect(screen.getByText('Serving 379 requests in the last 24h')).toBeInTheDocument()
    expect(screen.getByText('Traffic by provider')).toBeInTheDocument()
    expect(screen.getByText('Needs attention')).toBeInTheDocument()
    expect(screen.getByText('3 errors in the last 24h')).toBeInTheDocument()
  })
})
