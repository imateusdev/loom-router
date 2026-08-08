// The Agents screen is a catalogue, not a Codex feature list: the roles come
// from across the coding-agent ecosystem and picking one writes a Codex agent
// into ~/.codex/agents. These tests pin the parts a redesign could quietly
// lose - that the search covers both lists, that the copy says where a picked
// role ends up, and that a role already installed is not offered twice.

import { render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it, vi } from 'vitest'
import type { AgentInfo, AgentTemplate } from '@/types'

const agents: AgentInfo[] = [
  {
    name: 'code_reviewer',
    description: 'Use for read-only code review.',
    model: 'deepseek/deepseek-chat',
    effort: 'high',
    sandbox_mode: 'read-only',
    instructions: 'Review code changes.',
  },
]

const templates: AgentTemplate[] = [
  { id: 'code_reviewer', label: 'Code Reviewer', category: 'review', blurb: 'Read-only code review.', description: 'Use for review.', instructions: 'x', sandbox_mode: 'read-only' },
  { id: 'adversarial_critic', label: 'Adversarial Critic', category: 'review', blurb: 'Tries to refute a change.', description: 'Use to attack a design.', instructions: 'x', sandbox_mode: 'read-only' },
  { id: 'implementation_planner', label: 'Implementation Planner', category: 'build', blurb: 'Turns a goal into a plan.', description: 'Use to plan.', instructions: 'x', sandbox_mode: 'read-only' },
  { id: 'data_analyst', label: 'Data Analyst', category: 'data', blurb: 'Queries and summarizes data.', description: 'Use for data.', instructions: 'x', sandbox_mode: 'read-only' },
]

const apiMocks = vi.hoisted(() => ({
  upsert: vi.fn<() => Promise<void>>(() => Promise.resolve()),
  del: vi.fn<() => Promise<void>>(() => Promise.resolve()),
}))

vi.mock('@/lib/events', () => ({ useBackendState: () => {} }))
vi.mock('@/lib/api', () => ({
  isTauri: false,
  api: {
    agentsList: () => Promise.resolve(agents),
    agentTemplates: () => Promise.resolve(templates),
    multiAgentStatus: () => Promise.resolve(true),
    getConfig: () =>
      Promise.resolve({ port: 4180, providers: {}, side_call_fallback: null, native_slug_mode: false }),
    agentsUpsert: apiMocks.upsert,
    agentsDelete: apiMocks.del,
  },
}))

import AgentsPage from './Agents'

const section = async (name: RegExp) => {
  const heading = await screen.findByRole('heading', { name })
  return heading.parentElement as HTMLElement
}
const cardsIn = (el: HTMLElement) => el.querySelectorAll('[class*="h-full"]')
const searchBox = () => screen.getByRole('textbox', { name: /search/i })

describe('catalogue framing', () => {
  it('says where a picked role ends up', async () => {
    render(<AgentsPage />)
    // The whole point of the copy change: "Use" is an import into Codex, and
    // the roles are not Codex-specific.
    const blurb = await screen.findByText(/writes it into ~\/\.codex\/agents/i)
    expect(blurb).toBeInTheDocument()
    expect(blurb).toHaveTextContent(/not just codex/i)
  })

  it('labels each role with its category', async () => {
    render(<AgentsPage />)
    const catalog = await section(/agent catalogue/i)
    expect(within(catalog).getByText('Build')).toBeInTheDocument()
    expect(within(catalog).getByText('Data')).toBeInTheDocument()
  })

  it('does not offer a role that is already installed', async () => {
    render(<AgentsPage />)
    const catalog = await section(/agent catalogue/i)
    // "code_reviewer" exists as an agent, so re-using it would overwrite silently.
    expect(within(catalog).queryByText('Code Reviewer')).not.toBeInTheDocument()
    expect(within(catalog).getByText('Adversarial Critic')).toBeInTheDocument()
  })

  it('offers the import action on every catalogue card', async () => {
    render(<AgentsPage />)
    const catalog = await section(/agent catalogue/i)
    const uses = within(catalog).getAllByRole('button', { name: /^use$/i })
    expect(uses).toHaveLength(3)
    expect(uses[0]).toHaveAttribute('title', expect.stringMatching(/~\/\.codex\/agents/))
  })
})

describe('search', () => {
  it('filters the catalogue', async () => {
    const user = userEvent.setup()
    render(<AgentsPage />)
    await screen.findByRole('heading', { name: /agent catalogue/i })

    await user.type(searchBox(), 'plan')
    await waitFor(async () =>
      expect(cardsIn(await section(/agent catalogue/i))).toHaveLength(1),
    )
    expect(screen.getByText('Implementation Planner')).toBeInTheDocument()
  })

  it('matches on the category, not just the name', async () => {
    const user = userEvent.setup()
    render(<AgentsPage />)
    await screen.findByRole('heading', { name: /agent catalogue/i })

    // "data" is nowhere in the Data Analyst blurb except the category.
    await user.type(searchBox(), 'data')
    await waitFor(() => expect(screen.getByText('Data Analyst')).toBeInTheDocument())
  })

  it('filters the installed agents too', async () => {
    const user = userEvent.setup()
    render(<AgentsPage />)
    await screen.findByRole('heading', { name: /your agents/i })

    await user.type(searchBox(), 'planner')
    // The one installed agent is code_reviewer, so its section empties out.
    await waitFor(() =>
      expect(screen.queryByRole('heading', { name: /your agents/i })).not.toBeInTheDocument(),
    )
  })

  it('says so when nothing matches', async () => {
    const user = userEvent.setup()
    render(<AgentsPage />)
    await screen.findByRole('heading', { name: /agent catalogue/i })

    await user.type(searchBox(), 'zzzzz')
    expect(await screen.findByText(/no agent or role matches/i)).toBeInTheDocument()
  })
})

describe('overwrite and failure safety', () => {
  it('refuses to silently overwrite an existing agent, case-insensitively', async () => {
    const user = userEvent.setup()
    render(<AgentsPage />)
    await screen.findByRole('heading', { name: /your agents/i })
    apiMocks.upsert.mockClear()

    await user.click(screen.getByRole('button', { name: /add agent/i }))
    // The installed agent is "code_reviewer"; "Code_Reviewer" is the same file on a
    // case-insensitive filesystem, so this must be blocked, not written.
    // (Placeholder query: the form labels are not htmlFor-associated yet.)
    await user.type(screen.getByPlaceholderText('Name'), 'Code_Reviewer')
    await user.click(screen.getByRole('button', { name: /^save$/i }))

    expect(await screen.findByText(/already exists/i)).toBeInTheDocument()
    expect(apiMocks.upsert).not.toHaveBeenCalled()
  })

  it('surfaces a failed delete instead of dying silently', async () => {
    const user = userEvent.setup()
    apiMocks.del.mockRejectedValueOnce(new Error('permission denied'))
    render(<AgentsPage />)
    const installed = await section(/your agents/i)

    await user.click(within(installed).getByRole('button', { name: /delete/i }))
    const dialog = await screen.findByRole('dialog')
    await user.click(within(dialog).getByRole('button', { name: /^delete$/i }))

    expect(await within(dialog).findByText(/permission denied/i)).toBeInTheDocument()
    // The dialog stays open so the user can retry or cancel.
    expect(screen.getByRole('dialog')).toBeInTheDocument()
  })
})
