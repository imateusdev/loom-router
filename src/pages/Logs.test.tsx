// Two behaviours that were added because the page misbehaved: an upstream
// error used to wrap and blow the row height out, and the auto-refresh chip
// was a static badge that stated a behaviour the user could not influence.

import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { RequestEntry } from '@/types'

const LONG_ERROR =
  'native upstream returned 400 Bad Request: {"detail":"Unsupported parameter: \'reasoning.effort\' is not supported with this model.","type":"invalid_request_error"}'

const rows: RequestEntry[] = [
  {
    ts: 1_785_800_000,
    provider: 'kimi-coding',
    model: 'kimi-coding/k3',
    transport: 'ws',
    kind: 'request',
    status: 'ok',
    error: null,
    latency_ms: 1240,
    input_tokens: 12_400,
    output_tokens: 1_900,
    cached_tokens: 9_800,
    cost_usd: null,
  },
  {
    ts: 1_785_799_000,
    provider: 'codex-native',
    model: 'gpt-5.5',
    transport: 'http',
    kind: 'request',
    status: 'error',
    error: LONG_ERROR,
    latency_ms: 310,
    input_tokens: 0,
    output_tokens: 0,
    cached_tokens: 0,
    cost_usd: null,
  },
]

const visualRows = [
  {
    ...rows[0],
    visual_assistance: {
      images: [
        { model: 'vision/primary', attempts: 1, duration_ms: 840, cache_hit: false },
        { model: 'vision/fallback', attempts: 2, duration_ms: 1_700, cache_hit: true },
      ],
    },
  },
  {
    ...rows[0],
    ts: rows[0].ts - 1,
    visual_assistance: {
      images: [{ model: 'vision/fallback', attempts: 1, duration_ms: 420, cache_hit: false }],
    },
  },
  {
    ...rows[0],
    ts: rows[0].ts - 2,
    visual_assistance: {
      images: [{ model: 'vision/primary', attempts: 0, duration_ms: 0, cache_hit: true }],
    },
  },
  {
    ...rows[1],
    ts: rows[1].ts - 1,
    error: 'visual assistance exhausted configured fallbacks: provider unavailable',
  },
] as RequestEntry[]

let currentRows = rows
const recentRequests = vi.fn(() => Promise.resolve(currentRows))

vi.mock('@/lib/events', () => ({ useBackendState: () => {} }))
vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    recentRequests: () => recentRequests(),
    getConfig: () =>
      Promise.resolve({
        port: 4180,
        providers: {},
        side_call_fallback: null,
        native_slug_mode: false,
      }),
  },
}))

import LogsPage from './Logs'

beforeEach(() => {
  currentRows = rows
  recentRequests.mockClear()
})

describe('upstream error cell', () => {
  it('is clamped to one line and carries the full text for hover', async () => {
    render(<LogsPage />)
    const error = await screen.findByRole('button', { name: /native upstream returned 400/i })
    expect(error).toHaveClass('truncate')
    expect(error).toHaveAttribute('title', LONG_ERROR)
    expect(error).toHaveAttribute('aria-expanded', 'false')
  })

  it('expands in place on click and collapses again', async () => {
    const user = userEvent.setup()
    render(<LogsPage />)
    const error = await screen.findByRole('button', { name: /native upstream returned 400/i })

    await user.click(error)
    await waitFor(() => expect(error).toHaveAttribute('aria-expanded', 'true'))
    // `whitespace-normal` matters: the table cell is nowrap and would keep
    // the expanded text on one overflowing line without it.
    expect(error).toHaveClass('whitespace-normal')
    expect(error).not.toHaveClass('truncate')

    await user.click(error)
    await waitFor(() => expect(error).toHaveAttribute('aria-expanded', 'false'))
    expect(error).toHaveClass('truncate')
  })
})

describe('refresh control', () => {
  it('is a button, not a label', async () => {
    render(<LogsPage />)
    expect(await screen.findByRole('button', { name: /refresh now/i })).toBeInTheDocument()
  })

  it('fetches on demand, on top of the interval', async () => {
    const user = userEvent.setup()
    render(<LogsPage />)
    const button = await screen.findByRole('button', { name: /refresh now/i })
    await waitFor(() => expect(recentRequests).toHaveBeenCalled())

    const before = recentRequests.mock.calls.length
    await user.click(button)
    await waitFor(() => expect(recentRequests.mock.calls.length).toBe(before + 1))
  })
})

describe('visual assistance provenance', () => {
  it('renders successful primary, fallback, and cached visual analysis without exposing exhausted-chain provenance', async () => {
    currentRows = visualRows
    render(<LogsPage />)

    expect(await screen.findAllByText(/Visual analysis/i)).toHaveLength(3)
    expect(screen.getAllByText(/vision\/primary/i)).toHaveLength(2)
    expect(screen.getAllByText(/vision\/fallback/i)).toHaveLength(2)
    expect(screen.getAllByText(/cache miss/i)).toHaveLength(2)
    expect(screen.getAllByText(/cache hit/i)).toHaveLength(2)
    expect(screen.getAllByText(/1 attempt/i)).toHaveLength(2)
    expect(screen.getByText(/2 attempts/i)).toBeInTheDocument()
    expect(screen.getByText(/840ms/i)).toBeInTheDocument()
    expect(screen.getByText(/1\.7s/i)).toBeInTheDocument()
    expect(screen.queryByText(/Visual analysis:.*exhausted/i)).not.toBeInTheDocument()
    expect(screen.queryByText(/data:image|sk-/i)).not.toBeInTheDocument()
  })
})
