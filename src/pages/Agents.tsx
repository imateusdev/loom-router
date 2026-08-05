import { useEffect, useState } from 'react'
import { Pencil, Plus, Trash2 } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import type { AgentInfo, AppConfig } from '@/types'
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
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [error, setError] = useState<string | null>(null)

  const reload = () =>
    Promise.all([api.agentsList(), api.getConfig()])
      .then(([list, cfg]) => {
        setAgents(list)
        setConfig(cfg)
      })
      .catch((e) => setError(String(e)))

  useEffect(() => {
    reload()
  }, [])

  return (
    <div className="p-8 max-w-4xl">
      <div className="flex items-start justify-between mb-6">
        <div>
          <h2 className="text-2xl font-semibold tracking-tight">{s.agents.title}</h2>
          <p className="text-sm text-muted-foreground mt-1">
            {error ?? s.agents.subtitle}
          </p>
        </div>
        <AgentDialog models={enabledModelSlugs(config)} onSaved={reload} />
      </div>

      {!agents && !error && (
        <p className="text-sm text-muted-foreground">{s.common.loading}</p>
      )}
      <div className="space-y-4">
        {agents?.map((a) => (
          <AgentCard key={a.name} agent={a} models={enabledModelSlugs(config)} onChanged={reload} />
        ))}
        {agents?.length === 0 && (
          <p className="text-sm text-muted-foreground">{s.agents.noAgents}</p>
        )}
      </div>
    </div>
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
        <div className="flex items-center gap-3">
          <CardTitle className="text-base">{agent.name}</CardTitle>
          <Badge variant="secondary">{agent.model ?? s.agents.modelDefault}</Badge>
          <Badge variant="outline">
            {agent.effort
              ? { low: s.agents.effortLow, medium: s.agents.effortMedium, high: s.agents.effortHigh }[
                  agent.effort as 'low' | 'medium' | 'high'
                ] ?? agent.effort
              : s.agents.effortDefault}
          </Badge>
        </div>
        <div className="flex items-center gap-2">
          <AgentDialog agent={agent} models={models} onSaved={onChanged} />
          <DeleteAgentDialog agent={agent} onDeleted={onChanged} />
        </div>
      </CardHeader>
      {agent.instructions && (
        <CardContent>
          <p className="text-sm text-muted-foreground whitespace-pre-wrap line-clamp-3">
            {agent.instructions}
          </p>
        </CardContent>
      )}
    </Card>
  )
}

function AgentDialog({
  agent,
  models,
  onSaved,
}: {
  agent?: AgentInfo
  models: string[]
  onSaved: () => void
}) {
  const s = useStrings()
  const [open, setOpen] = useState(false)
  const [name, setName] = useState(agent?.name ?? '')
  const [model, setModel] = useState(agent?.model ?? DEFAULT_SENTINEL)
  const [effort, setEffort] = useState(agent?.effort ?? DEFAULT_SENTINEL)
  const [instructions, setInstructions] = useState(agent?.instructions ?? '')
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
        model: model === DEFAULT_SENTINEL ? null : model,
        effort: effort === DEFAULT_SENTINEL ? null : effort,
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
        {agent ? (
          <Button variant="ghost" size="icon" title={s.agents.edit}>
            <Pencil className="h-4 w-4" />
          </Button>
        ) : (
          <Button>
            <Plus className="h-4 w-4 mr-2" />
            {s.agents.add}
          </Button>
        )}
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {agent ? `${s.agents.edit} — ${agent.name}` : s.agents.add}
          </DialogTitle>
        </DialogHeader>
        <div className="space-y-4 pt-2">
          <Input
            placeholder={s.agents.name}
            value={name}
            onChange={(e) => setName(e.target.value)}
            disabled={!!agent}
          />
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
          <textarea
            placeholder={s.agents.instructionsPlaceholder}
            value={instructions}
            onChange={(e) => setInstructions(e.target.value)}
            rows={5}
            className="placeholder:text-muted-foreground dark:bg-input/30 border-input w-full min-w-0 rounded-md border bg-transparent px-3 py-2 text-base shadow-xs transition-[color,box-shadow] outline-none focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px] disabled:pointer-events-none disabled:cursor-not-allowed disabled:opacity-50 md:text-sm"
          />
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
