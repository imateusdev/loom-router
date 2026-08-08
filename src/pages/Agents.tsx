import { useEffect, useState } from 'react'
import { AlertTriangle, Pencil, Plus, Trash2 } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import type { AgentInfo, AgentTemplate, AppConfig } from '@/types'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog'
import PageShell from '@/components/PageShell'

/// Same reflow as the shared CARD_GRID but stretched: on this screen the
/// cards are being compared against each other, and a row of cards reads as
/// a row only when they share a height. The shared grid keeps `items-start`
/// because its panels (a provider with 2 models next to one with 40) have no
/// business being padded to match.
const STRETCH_GRID = 'grid items-stretch gap-3 grid-cols-[repeat(auto-fit,minmax(280px,1fr))]'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'

// Radix Select forbids empty-string item values, so null is carried by a
// sentinel on the form side.
const DEFAULT_SENTINEL = '__default__'
const EFFORTS = ['low', 'medium', 'high'] as const
const SANDBOX_MODES = ['read-only', 'workspace-write'] as const

// Stable palette by tag hash: the same tag always keeps the same color.
const TAG_COLORS = [
  'bg-blue-100 text-blue-800 dark:bg-blue-900/40 dark:text-blue-300',
  'bg-emerald-100 text-emerald-800 dark:bg-emerald-900/40 dark:text-emerald-300',
  'bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300',
  'bg-rose-100 text-rose-800 dark:bg-rose-900/40 dark:text-rose-300',
  'bg-violet-100 text-violet-800 dark:bg-violet-900/40 dark:text-violet-300',
  'bg-cyan-100 text-cyan-800 dark:bg-cyan-900/40 dark:text-cyan-300',
  'bg-orange-100 text-orange-800 dark:bg-orange-900/40 dark:text-orange-300',
  'bg-pink-100 text-pink-800 dark:bg-pink-900/40 dark:text-pink-300',
]
function tagClass(tag: string): string {
  let h = 0
  for (const ch of tag) h = (h * 31 + ch.charCodeAt(0)) >>> 0
  return TAG_COLORS[h % TAG_COLORS.length]
}

// Enabled models from the config, exposed as "provider/model" slugs.
function enabledModelSlugs(config: AppConfig | null, nativeModels: string[]): string[] {
  if (!config) return nativeModels
  return [
    ...nativeModels,
    ...Object.values(config.providers).flatMap((p) =>
      p.models.filter((m) => m.enabled).map((m) => `${p.id}/${m.id}`),
    ),
  ]
}

export default function AgentsPage() {
  const s = useStrings()
  const [agents, setAgents] = useState<AgentInfo[] | null>(null)
  const [templates, setTemplates] = useState<AgentTemplate[]>([])
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [multiAgent, setMultiAgent] = useState<boolean | null>(null)
  const [nativeModels, setNativeModels] = useState<string[]>([])
  const [error, setError] = useState<string | null>(null)
  const [query, setQuery] = useState('')
  const [tagFilter, setTagFilter] = useState<string | null>(null)

  const reload = () =>
    Promise.all([
      api.agentsList(),
      api.getConfig(),
      api.agentTemplates(),
      api.multiAgentStatus(),
      api.codexNativeModels(),
    ])
      .then(([list, cfg, tpls, ma, native]) => {
        setAgents(list)
        setConfig(cfg)
        setTemplates(tpls)
        setMultiAgent(ma)
        setNativeModels(native)
      })
      .catch((e) => setError(String(e)))

  useEffect(() => {
    reload()
  }, [])

  // Templates whose suggested name is not already taken by an existing
  // agent - installing the same name twice would silently overwrite. The
  // comparison is case-insensitive because the agents dir lives on
  // case-insensitive filesystems (Windows, default macOS), where
  // `Code Reviewer.toml` and `code_reviewer.toml` are the same file.
  const availableTemplates = templates.filter(
    (t) => !agents?.some((a) => a.name.toLowerCase() === t.id.toLowerCase()),
  )

  // Match on everything the user can see plus the category, so "review"
  // finds the reviewer, the security auditor and the adversarial critic.
  const q = query.trim().toLowerCase()
  const matches = (...fields: (string | null | undefined)[]) =>
    !q || fields.some((f) => f?.toLowerCase().includes(q))

  const catalog = availableTemplates.filter((t) =>
    matches(t.label, t.blurb, t.description, t.category, t.id),
  )
  const allTags = [
    ...new Set((agents ?? []).flatMap((a) => a.tags ?? [])),
  ].sort((a, b) => a.localeCompare(b))
  const installed = (agents ?? []).filter(
    (a) =>
      (!tagFilter || (a.tags ?? []).includes(tagFilter)) &&
      matches(a.name, a.description, a.model, a.instructions, (a.tags ?? []).join(' ')),
  )

  return (
    <PageShell
      title={s.agents.title}
      subtitle={error ?? s.agents.subtitle}
      actions={
        <AgentDialog
          models={enabledModelSlugs(config, nativeModels)}
          existingNames={(agents ?? []).map((a) => a.name)}
          onSaved={reload}
        />
      }
    >
      {multiAgent === false && <MultiAgentBanner onEnabled={reload} />}

      {!agents && !error && (
        <p className="text-sm text-muted-foreground">{s.common.loading}</p>
      )}

      {/* One search over both lists: with a catalogue this size, scanning
          is the bottleneck, and a user looking for "review" does not care
          whether the match is already installed or not. */}
      <div className="mb-6">
        <Input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder={s.agents.searchPlaceholder}
          aria-label={s.agents.searchPlaceholder}
          className="max-w-sm"
        />
        <div className="mt-3 flex flex-wrap items-center gap-2">
            <button
              type="button"
              onClick={() => setTagFilter(null)}
              className={
                'rounded-full border px-2.5 py-1 text-xs ' +
                (tagFilter === null ? 'border-ring bg-accent text-accent-foreground' : 'text-muted-foreground hover:bg-accent')
              }
            >
              {s.agents.tagFilterAll}
            </button>
            {allTags.map((tag) => (
              <button
                key={tag}
                type="button"
                onClick={() => setTagFilter(tag === tagFilter ? null : tag)}
                className={
                  'rounded-full border px-2.5 py-1 text-xs ' +
                  (tag === tagFilter ? 'border-ring' : 'border-transparent hover:border-border')
                }
              >
                <span className={tagClass(tag)}>{tag}</span>
              </button>
            ))}
          </div>
      </div>

      {installed.length > 0 && (
        <section className="mb-8">
          <h3 className="mb-3 text-sm font-medium text-muted-foreground">
            {s.agents.installedTitle}
          </h3>
          {/* Stretch, not items-start: a row of cards the user is comparing
              reads as a row only when they are the same height. */}
          <div className={STRETCH_GRID}>
            {installed.map((a) => (
              <AgentCard key={a.name} agent={a} models={enabledModelSlugs(config, nativeModels)} onChanged={reload} />
            ))}
          </div>
        </section>
      )}

      {agents?.length === 0 && !query && (
        <p className="mb-8 text-sm text-muted-foreground">{s.agents.noAgents}</p>
      )}

      <section>
        <h3 className="text-sm font-medium text-muted-foreground">{s.agents.catalogTitle}</h3>
        <p className="mb-3 mt-1 max-w-2xl text-sm text-muted-foreground">
          {s.agents.catalogSubtitle}
        </p>
        {catalog.length === 0 ? (
          <p className="text-sm text-muted-foreground">{s.agents.noMatch}</p>
        ) : (
          <div className={STRETCH_GRID}>
            {catalog.map((t) => (
              <TemplateCard key={t.id} template={t} models={enabledModelSlugs(config, nativeModels)} onSaved={reload} />
            ))}
          </div>
        )}
      </section>
    </PageShell>
  )
}

function MultiAgentBanner({ onEnabled }: { onEnabled: () => void }) {
  const s = useStrings()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const enable = async () => {
    setBusy(true)
    setError(null)
    try {
      await api.setMultiAgent(true)
      onEnabled()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }
  return (
    <div className="mb-6 rounded-md border border-yellow-500/40 bg-yellow-500/10 px-4 py-3">
      <div className="flex items-center justify-between gap-4">
        <div className="flex items-center gap-3">
          <AlertTriangle className="h-4 w-4 text-yellow-500 shrink-0" />
          <p className="text-sm">{s.agents.multiAgentOff}</p>
        </div>
        <Button size="sm" onClick={enable} disabled={busy}>
          {s.agents.multiAgentEnable}
        </Button>
      </div>
      {error && <p className="text-sm text-destructive break-all mt-2">{error}</p>}
    </div>
  )
}

function TemplateCard({
  template,
  models,
  onSaved,
}: {
  template: AgentTemplate
  models: string[]
  onSaved: () => void
}) {
  const s = useStrings()
  return (
    // `h-full` + column layout so every card in a row is the same height and
    // the action sits on the same baseline, whatever the blurb's length.
    // Tighter vertical rhythm than the Card default (gap-6 py-6): these are
    // compact catalogue entries, and the default left ~70px of dead space
    // between the title and a one-line blurb.
    <Card className="flex h-full flex-col gap-3 py-4">
      <CardHeader className="space-y-0 pb-0">
        <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
          <CardTitle className="text-sm">{template.label}</CardTitle>
          <Badge variant="outline" className={tagClass(template.category) + ' shrink-0 border-transparent text-[11px] font-normal'}>
            {categoryLabel(s, template.category)}
          </Badge>
        </div>
      </CardHeader>
      <CardContent className="flex flex-1 flex-col gap-3">
        <p className="text-xs text-muted-foreground">{template.blurb}</p>
        {/* mt-auto pins the button to the bottom of the stretched card. */}
        <div className="mt-auto">
          <AgentDialog
            models={models}
            onSaved={onSaved}
            prefill={template}
            trigger={
              <Button size="sm" variant="outline" title={s.agents.useTemplateHint}>
                {s.agents.useTemplate}
              </Button>
            }
          />
        </div>
      </CardContent>
    </Card>
  )
}

/// Category labels are translated; the stored value is a stable slug.
function categoryLabel(s: ReturnType<typeof useStrings>, key: string): string {
  const map: Record<string, string> = {
    review: s.agents.catReview,
    build: s.agents.catBuild,
    investigate: s.agents.catInvestigate,
    quality: s.agents.catQuality,
    ship: s.agents.catShip,
    write: s.agents.catWrite,
    data: s.agents.catData,
    ops: s.agents.catOps,
  }
  return map[key] ?? key
}

function AgentCard({
  agent,
  models,
  onChanged,
}: {
  agent: AgentInfo
  models: string[]
  onChanged: () => void
}) {
  const s = useStrings()
  return (
    <Card className="flex h-full flex-col">
      <CardHeader className="flex-row items-center justify-between space-y-0">
        <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1.5">
          <CardTitle className="text-base">{agent.name}</CardTitle>
          <Badge
            variant="secondary"
            className="min-w-0 max-w-full truncate"
            title={agent.model ?? undefined}
          >
            {agent.model ?? s.agents.modelDefault}
          </Badge>
          <Badge variant="outline">
            {agent.effort
              ? { low: s.agents.effortLow, medium: s.agents.effortMedium, high: s.agents.effortHigh }[
                  agent.effort as 'low' | 'medium' | 'high'
                ] ?? agent.effort
              : s.agents.effortDefault}
          </Badge>
          {agent.sandbox_mode && (
            <Badge variant="outline">
              {agent.sandbox_mode === 'read-only'
                ? s.agents.sandboxReadOnly
                : s.agents.sandboxWorkspaceWrite}
            </Badge>
          )}
          {(agent.tags ?? []).map((tag) => (
            <Badge key={tag} className={tagClass(tag) + ' border-transparent'}>
              {tag}
            </Badge>
          ))}
        </div>
        <div className="flex items-center gap-2">
          <AgentDialog agent={agent} models={models} onSaved={onChanged} />
          <DeleteAgentDialog agent={agent} onDeleted={onChanged} />
        </div>
      </CardHeader>
      <CardContent className="space-y-2">
        {agent.description && (
          <p className="text-sm">{agent.description}</p>
        )}
        {agent.instructions && (
          <p className="text-sm text-muted-foreground whitespace-pre-wrap line-clamp-3" title={agent.instructions}>
            {agent.instructions}
          </p>
        )}
      </CardContent>
    </Card>
  )
}

function AgentDialog({
  agent,
  prefill,
  models,
  existingNames,
  onSaved,
  trigger,
}: {
  agent?: AgentInfo
  // A template whose fields prefill the form (name stays editable).
  prefill?: AgentTemplate
  models: string[]
  // Names already in ~/.codex/agents, so a fresh dialog can refuse to
  // silently overwrite one. Only checked when creating (no `agent` prop);
  // editing keeps the same name by construction.
  existingNames?: string[]
  onSaved: () => void
  trigger?: React.ReactNode
}) {
  const s = useStrings()
  const [open, setOpen] = useState(false)
  const [name, setName] = useState(agent?.name ?? prefill?.id ?? '')
  const [description, setDescription] = useState(agent?.description ?? prefill?.description ?? '')
  const [model, setModel] = useState(agent?.model ?? DEFAULT_SENTINEL)
  const [effort, setEffort] = useState(agent?.effort ?? DEFAULT_SENTINEL)
  const [sandbox, setSandbox] = useState(agent?.sandbox_mode ?? prefill?.sandbox_mode ?? DEFAULT_SENTINEL)
  const [instructions, setInstructions] = useState(agent?.instructions ?? prefill?.instructions ?? '')
  const [tagsText, setTagsText] = useState((agent?.tags ?? []).join(', '))
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Keep the previously picked model selectable even when it is no longer
  // enabled, so editing an agent doesn't silently drop its model.
  const modelOptions = models.includes(model) || model === DEFAULT_SENTINEL
    ? models
    : [model, ...models]

  const save = async () => {
    if (!name.trim()) {
      setError(s.agents.nameRequired)
      return
    }
    // Creating under a taken name would overwrite that agent's file with no
    // warning (the upsert is a patch, not a create). Case-insensitive: the
    // agents dir is case-insensitive on Windows and default macOS. The edit
    // flow is exempt - its name field is disabled anyway.
    if (
      !agent &&
      existingNames?.some((n) => n.toLowerCase() === name.trim().toLowerCase())
    ) {
      setError(s.agents.nameTaken.replace('{{name}}', name.trim()))
      return
    }
    setError(null)
    setBusy(true)
    try {
      await api.agentsUpsert({
        name: name.trim(),
        description: description.trim(),
        model: model === DEFAULT_SENTINEL ? null : model,
        effort: effort === DEFAULT_SENTINEL ? null : effort,
        sandbox_mode: sandbox === DEFAULT_SENTINEL ? null : sandbox,
        instructions,
        tags: tagsText.split(',').map((t) => t.trim()).filter(Boolean),
      })
      setOpen(false)
      onSaved()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        {trigger ?? (agent ? (
          <Button variant="ghost" size="icon" title={s.agents.edit}>
            <Pencil className="h-4 w-4" />
          </Button>
        ) : (
          <Button>
            <Plus className="h-4 w-4 mr-2" />
            {s.agents.add}
          </Button>
        ))}
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {agent ? `${s.agents.edit} - ${agent.name}` : s.agents.add}
          </DialogTitle>
        </DialogHeader>
        <div className="space-y-4 pt-2">
          <div className="space-y-1.5">
            <label className="text-sm font-medium">{s.agents.name}</label>
            <Input
              placeholder={s.agents.name}
              value={name}
              onChange={(e) => setName(e.target.value)}
              disabled={!!agent}
            />
          </div>
          <div className="space-y-1.5">
            <label className="text-sm font-medium">{s.agents.description}</label>
            <Input
              placeholder={s.agents.descriptionPlaceholder}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
            <p className="text-xs text-muted-foreground">{s.agents.descriptionHint}</p>
          </div>
          <div className="space-y-1.5">
            <label className="text-sm font-medium">{s.agents.model}</label>
            <Select value={model} onValueChange={setModel}>
              <SelectTrigger>
                <SelectValue placeholder={s.agents.model} />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={DEFAULT_SENTINEL}>{s.agents.modelDefault}</SelectItem>
                {modelOptions.map((m) => (
                  <SelectItem key={m} value={m}>
                    {m}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="grid grid-cols-2 gap-3 [&>*]:min-w-0">
            <div className="space-y-1.5">
              <label className="text-sm font-medium">{s.agents.effort}</label>
              <Select value={effort} onValueChange={setEffort}>
                <SelectTrigger>
                  <SelectValue placeholder={s.agents.effort} />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={DEFAULT_SENTINEL}>{s.agents.effortDefault}</SelectItem>
                  {EFFORTS.map((e) => (
                    <SelectItem key={e} value={e}>
                      {{ low: s.agents.effortLow, medium: s.agents.effortMedium, high: s.agents.effortHigh }[e]}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1.5">
              <label className="text-sm font-medium">{s.agents.sandbox}</label>
              <Select value={sandbox} onValueChange={setSandbox}>
                <SelectTrigger>
                  <SelectValue placeholder={s.agents.sandbox} />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={DEFAULT_SENTINEL}>{s.agents.sandboxInherit}</SelectItem>
                  {SANDBOX_MODES.map((m) => (
                    <SelectItem key={m} value={m}>
                      {m === 'read-only' ? s.agents.sandboxReadOnly : s.agents.sandboxWorkspaceWrite}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>
          <div className="space-y-1.5">
            <label className="text-sm font-medium">{s.agents.tags}</label>
            <Input
              placeholder={s.agents.tagsPlaceholder}
              value={tagsText}
              onChange={(e) => setTagsText(e.target.value)}
            />
          </div>
          <div className="space-y-1.5">
            <label className="text-sm font-medium">{s.agents.instructions}</label>
            <textarea
              placeholder={s.agents.instructionsPlaceholder}
              value={instructions}
              onChange={(e) => setInstructions(e.target.value)}
              rows={5}
              className="placeholder:text-muted-foreground dark:bg-input/30 border-input w-full min-w-0 rounded-md border bg-transparent px-3 py-2 text-sm shadow-xs transition-[color,box-shadow] outline-none focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px] disabled:pointer-events-none disabled:cursor-not-allowed disabled:opacity-50"
            />
          </div>
          {error && <p className="text-sm text-destructive break-all">{error}</p>}
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={() => setOpen(false)}>
              {s.agents.cancel}
            </Button>
            <Button onClick={save} disabled={busy}>
              {s.agents.save}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}

function DeleteAgentDialog({
  agent,
  onDeleted,
}: {
  agent: AgentInfo
  onDeleted: () => void
}) {
  const s = useStrings()
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const confirm = async () => {
    setBusy(true)
    setError(null)
    try {
      await api.agentsDelete(agent.name)
      setOpen(false)
      onDeleted()
    } catch (e) {
      // A failed delete (permissions, locked file) must surface here -
      // swallowing it leaves the dialog open with no clue what happened.
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="ghost" size="icon" title={s.agents.delete}>
          <Trash2 className="h-4 w-4 text-destructive" />
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{s.agents.deleteTitle}</DialogTitle>
        </DialogHeader>
        <p className="text-sm text-muted-foreground pt-2">
          {s.agents.deleteConfirm.replace('{{name}}', agent.name)}
        </p>
        {error && <p className="text-sm text-destructive break-all pt-2">{error}</p>}
        <div className="flex justify-end gap-2 pt-4">
          <Button variant="outline" onClick={() => setOpen(false)}>
            {s.agents.cancel}
          </Button>
          <Button variant="destructive" onClick={confirm} disabled={busy}>
            {s.agents.delete}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}
