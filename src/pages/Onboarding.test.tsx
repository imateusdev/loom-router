// The first-run flow, as specified:
//
//   1. a welcome screen with a single "Start" button;
//   2. a step-by-step: activate the Codex integration, then report whether
//      it actually took, then providers with the option to skip;
//   3. the app itself.
//
// The proxy is deliberately absent from that list: it autostarts with the
// app (`run()` in lib.rs), so the walkthrough only chooses what to route
// through it. The welcome copy states the port to make that visible.

import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it, vi } from 'vitest'

let managedBlock = false
let applyFails = false
const codexApply = vi.fn(() => {
  if (applyFails) return Promise.reject(new Error('nope'))
  managedBlock = true
  return Promise.resolve()
})
const completeOnboarding = vi.fn(() => Promise.resolve())
const navigate = vi.fn()

vi.mock('react-router', () => ({ useNavigate: () => navigate }))
vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    serverStatus: () => Promise.resolve({ running: true, port: 4180, url: null }),
    getConfig: () =>
      Promise.resolve({ port: 4180, providers: {}, side_call_fallback: null, native_slug_mode: false }),
    codexStatus: () =>
      Promise.resolve({
        codex_home: '~/.codex',
        config_exists: true,
        managed_block_present: managedBlock,
        managed_block_orphaned: false,
        native_catalog_present: managedBlock,
        merged_catalog_present: managedBlock,
        merged_model_count: 0,
        codex_cli_available: true,
        integration_enabled: managedBlock,
      }),
    multiAgentStatus: () => Promise.resolve(false),
    setMultiAgent: (v: boolean) => Promise.resolve(v),
    codexApply: () => codexApply(),
    completeOnboarding: () => completeOnboarding(),
  },
}))

import Onboarding from './Onboarding'

const onDone = vi.fn()
const renderFlow = () => render(<Onboarding onDone={onDone} />)

beforeEach(() => {
  managedBlock = false
  applyFails = false
  // These live at module scope so the vi.mock factory can close over them;
  // their call history has to be cleared per test explicitly.
  codexApply.mockClear()
  completeOnboarding.mockClear()
  navigate.mockClear()
  onDone.mockClear()
})

describe('1. welcome screen', () => {
  it('offers exactly one action, and it is Start', async () => {
    renderFlow()
    const start = await screen.findByRole('button', { name: /^start$/i })
    expect(start).toBeInTheDocument()
    // The language picker is a combobox, not a button; Start must be the
    // only thing to press.
    expect(screen.getAllByRole('button')).toHaveLength(1)
  })

  it('says the proxy is already running, and on which port', async () => {
    renderFlow()
    expect(await screen.findByText(/already running on port 4180/i)).toBeInTheDocument()
  })

  it('still offers the language choice', async () => {
    renderFlow()
    expect(await screen.findByRole('combobox')).toBeInTheDocument()
  })
})

describe('2. step-by-step', () => {
  it('opens on the Codex step after Start', async () => {
    const user = userEvent.setup()
    renderFlow()
    await user.click(await screen.findByRole('button', { name: /^start$/i }))
    expect(await screen.findByRole('heading', { name: /connect codex/i })).toBeInTheDocument()
    expect(screen.getByText(/step 1 of/i)).toBeInTheDocument()
  })

  it('reports "Integration active" only when the block really landed', async () => {
    const user = userEvent.setup()
    renderFlow()
    await user.click(await screen.findByRole('button', { name: /^start$/i }))
    await user.click(await screen.findByRole('button', { name: /activate integration/i }))

    // Success is a confirmation read, not "the call returned".
    expect(await screen.findByText(/integration active/i)).toBeInTheDocument()
    expect(codexApply).toHaveBeenCalled()
  })

  it('reports failure instead of a false success', async () => {
    applyFails = true
    const user = userEvent.setup()
    renderFlow()
    await user.click(await screen.findByRole('button', { name: /^start$/i }))
    await user.click(await screen.findByRole('button', { name: /activate integration/i }))

    expect(await screen.findByRole('button', { name: /try again/i })).toBeInTheDocument()
    expect(screen.queryByText(/integration active/i)).not.toBeInTheDocument()
  })

  it('lets the Codex step be skipped', async () => {
    const user = userEvent.setup()
    renderFlow()
    await user.click(await screen.findByRole('button', { name: /^start$/i }))
    await user.click(await screen.findByRole('button', { name: /skip for now/i }))
    expect(await screen.findByRole('heading', { name: /add a provider/i })).toBeInTheDocument()
  })

  it('reaches the providers step, which is skippable', async () => {
    const user = userEvent.setup()
    renderFlow()
    await user.click(await screen.findByRole('button', { name: /^start$/i }))
    await user.click(await screen.findByRole('button', { name: /skip for now/i }))

    expect(await screen.findByRole('heading', { name: /add a provider/i })).toBeInTheDocument()
    // Skippable: advancing does not require adding one.
    expect(screen.getByRole('button', { name: /skip for now/i })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /add a provider/i })).toBeInTheDocument()
  })

  it('can go back a step', async () => {
    const user = userEvent.setup()
    renderFlow()
    await user.click(await screen.findByRole('button', { name: /^start$/i }))
    await user.click(await screen.findByRole('button', { name: /skip for now/i }))
    await user.click(await screen.findByRole('button', { name: /back/i }))
    expect(await screen.findByRole('heading', { name: /connect codex/i })).toBeInTheDocument()
  })
})

describe('3. into the app', () => {
  it('records completion before leaving, and lands in the app', async () => {
    const user = userEvent.setup()
    renderFlow()
    await user.click(await screen.findByRole('button', { name: /^start$/i }))
    await user.click(await screen.findByRole('button', { name: /skip for now/i }))
    // providers -> agents
    await user.click(await screen.findByRole('button', { name: /skip for now/i }))
    await user.click(await screen.findByRole('button', { name: /finish/i }))

    // Persist first: a failed write should replay the walkthrough rather
    // than strand someone in a half-finished setup.
    await waitFor(() => expect(completeOnboarding).toHaveBeenCalled())
    expect(onDone).toHaveBeenCalled()
    expect(navigate).toHaveBeenCalledWith('/')
  })

  it('does not mark it done if the user only opened it', async () => {
    renderFlow()
    await screen.findByRole('button', { name: /^start$/i })
    expect(completeOnboarding).not.toHaveBeenCalled()
  })
})
