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
import PageShell, { CARD_GRID } from '@/components/PageShell'
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

// Enabled models from the config, exposed as "provider/model" slugs.
function enabledModelSlugs(config: AppConfig | null): string[] {
  if (!config) return []
  return Object.values(config.providers).flatMap((p) =>
    p.models.filter((m) => m.enabled).map((m) => `${p.id}/${m.id}`),
  )
}

export default function AgentsPage() {
  const s = useStrings()
  const [agents, setAgents] = useState<AgentInfo[] | null>(null)
  const [templates, setTemplates] = useState<AgentTemplate[]>([])
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [multiAgent, setMultiAgent] = useState<boolean | null>(null)
  const [error, setError] = useState<string | null>(null)

  const reload = () =>
    Promise.all([api.agentsList(), api.getConfig(), api.agentTemplates(), api.multiAgentStatus()])
      .then(([list, cfg, tpls, ma]) => {
        setAgents(list)
        setConfig(cfg)
        setTemplates(tpls)
        setMultiAgent(ma)
      })
      .catch((e) => setError(String(e)))

  useEffect(() => {
    reload()
  }, [])

  // Templates whose suggested name is not already taken by an existing
  // agent — installing the same name twice would silently overwrite.
  const availableTemplates = templates.filter(
    (t) => !agents?.some((a) => a.name === t.id),
  )

  return (
    <PageShell
      title={s.agents.title}
      subtitle={error ?? s.agents.subtitle}
      actions={<AgentDialog models={enabledModelSlugs(config)} onSaved={reload} />}
    >
      {multiAgent === false && <MultiAgentBanner onEnabled={reload} />}

      {!agents && !error && (
        <p className="text-sm text-muted-foreground">{s.common.loading}</p>
      )}

      {availableTemplates.length > 0 && (
        <div className="mb-6">
          <h3 className="text-sm font-medium text-muted-foreground mb-3">
            {s.agents.templatesTitle}
          </h3>
          {/* Was a hard `grid-cols-2`, which squeezed two templates into a
              narrow window and left the rest of a wide one empty. */}
          <div className="grid grid-cols-[repeat(auto-fit,minmax(260px,1fr))] items-start gap-3">
            {availableTemplates.map((t) => (
              <TemplateCard key={t.id} template={t} models={enabledModelSlugs(config)} onSaved={reload} />
            ))}
          </div>
        </div>
      )}

      {agents?.length === 0 ? (
        <p className="text-sm text-muted-foreground">{s.agents.noAgents}</p>
      ) : (
        <div className={CARD_GRID}>
          {agents?.map((a) => (
            <AgentCard key={a.name} agent={a} models={enabledModelSlugs(config)} onChanged={reload} />
          ))}
        </div>
      )}
    </PageShell>
  )
}

function MultiAgentBanner({ onEnabled }: { onEnabled: () => void }) {
  const s = useStrings()
  const [busy, setBusy] = useState(false)
  const enable = async () => {
    setBusy(true)
    try {
      await api.setMultiAgent(true)
      onEnabled()
    } finally {
      setBusy(false)
    }
  }
  return (
    <div className="mb-6 flex items-center justify-between gap-4 rounded-md border border-yellow-500/40 bg-yellow-500/10 px-4 py-3">
      <div className="flex items-center gap-3">
        <AlertTriangle className="h-4 w-4 text-yellow-500 shrink-0" />
        <p className="text-sm">{s.agents.multiAgentOff}</p>
      </div>
      <Button size="sm" onClick={enable} disabled={busy}>
        {s.agents.multiAgentEnable}
      </Button>
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
    <Card>
      <CardHeader className="flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-sm">{template.label}</CardTitle>
        <AgentDialog
          models={models}
          onSaved={onSaved}
          prefill={template}
          trigger={
            <Button size="sm" variant="outline">
              {s.agents.useTemplate}
            </Button>
          }
        />
      </CardHeader>
      <CardContent>
        <p className="text-xs text-muted-foreground">{template.blurb}</p>
      </CardContent>
    </Card>
  )
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
    <Card>
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
          <p className="text-sm text-muted-foreground whitespace-pre-wrap line-clamp-3">
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
  onSaved,
  trigger,
}: {
  agent?: AgentInfo
  // A template whose fields prefill the form (name stays editable).
  prefill?: AgentTemplate
  models: string[]
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
            {agent ? `${s.agents.edit} — ${agent.name}` : s.agents.add}
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

  const confirm = async () => {
    setBusy(true)
    try {
      await api.agentsDelete(agent.name)
      setOpen(false)
      onDeleted()
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
