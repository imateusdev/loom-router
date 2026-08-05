import { useEffect, useState } from 'react'
import { Pencil, Plus, RefreshCw, Trash2 } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import { PRESETS, type AppConfig, type Provider } from '@/types'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Badge } from '@/components/ui/badge'
import { Checkbox } from '@/components/ui/checkbox'
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

export default function ProvidersPage() {
  const s = useStrings()
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [error, setError] = useState<string | null>(null)

  const reload = () =>
    api
      .getConfig()
      .then(setConfig)
      .catch((e) => setError(String(e)))

  useEffect(() => {
    reload()
  }, [])

  if (error) return <PageShell title={s.providers.title} subtitle={String(error)}>{null}</PageShell>
  if (!config) return <PageShell title={s.providers.title} subtitle={s.common.loading}>{null}</PageShell>

  const providers = Object.values(config.providers)

  return (
    <PageShell
      title={s.providers.title}
      subtitle={s.providers.subtitle}
      actions={<AddProviderDialog onSaved={reload} />}
    >
      <div className="space-y-4">
        {providers.map((p) => (
          <ProviderCard key={p.id} provider={p} onChanged={reload} />
        ))}
        {providers.length === 0 && (
          <p className="text-sm text-muted-foreground">{s.providers.noProviders}</p>
        )}
      </div>
    </PageShell>
  )
}

function PageShell({
  title,
  subtitle,
  actions,
  children,
}: {
  title: string
  subtitle: string
  actions?: React.ReactNode
  children: React.ReactNode
}) {
  return (
    <div className="p-8 max-w-4xl">
      <div className="flex items-start justify-between mb-6">
        <div>
          <h2 className="text-2xl font-semibold tracking-tight">{title}</h2>
          <p className="text-sm text-muted-foreground mt-1">{subtitle}</p>
        </div>
        {actions}
      </div>
      {children}
    </div>
  )
}

function AddProviderDialog({ onSaved }: { onSaved: () => void }) {
  const s = useStrings()
  const [open, setOpen] = useState(false)
  const [presetId, setPresetId] = useState<string>(PRESETS[0].id)
  const [custom, setCustom] = useState(false)
  const [name, setName] = useState('')
  const [baseUrl, setBaseUrl] = useState('')
  const [apiKey, setApiKey] = useState('')
  const [validating, setValidating] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const save = async () => {
    const preset = PRESETS.find((p) => p.id === presetId)!
    const built: Provider = custom
      ? {
          id: name.toLowerCase().replace(/[^a-z0-9]+/g, '-') || 'custom',
          name: name || 'Custom',
          protocol: 'openai',
          base_url: baseUrl,
          api_key: apiKey || null,
          user_agent: null,
          models: [],
          enabled: true,
        }
      : {
          id: preset.id,
          name: preset.name,
          protocol: preset.protocol,
          base_url: preset.base_url,
          api_key: apiKey || null,
          user_agent: preset.userAgent ?? null,
          models: (preset.defaultModels ?? []).map((id) => ({ id, enabled: true })),
          enabled: true,
        }
    setError(null)
    setValidating(true)
    try {
      // Validate the key and seed the model list in one call.
      const ids = await api.validateProvider(built)
      if (ids.length > 0) {
        const existing = new Map(built.models.map((m) => [m.id, m]))
        built.models = ids.map((id) => existing.get(id) ?? { id, enabled: false })
      }
      await api.saveProvider(built)
      setOpen(false)
      onSaved()
    } catch (e) {
      // Endpoints without a /models route (e.g. Kimi Coding Plan) can't be
      // validated this way; keep the default models and offer Save anyway.
      setError(`${s.providers.validationFailed}: ${String(e)}`)
    } finally {
      setValidating(false)
    }
  }

  const saveAnyway = async () => {
    const preset = PRESETS.find((p) => p.id === presetId)!
    const built: Provider = custom
      ? {
          id: name.toLowerCase().replace(/[^a-z0-9]+/g, '-') || 'custom',
          name: name || 'Custom',
          protocol: 'openai',
          base_url: baseUrl,
          api_key: apiKey || null,
          user_agent: null,
          models: [],
          enabled: true,
        }
      : {
          id: preset.id,
          name: preset.name,
          protocol: preset.protocol,
          base_url: preset.base_url,
          api_key: apiKey || null,
          user_agent: preset.userAgent ?? null,
          models: (preset.defaultModels ?? []).map((id) => ({ id, enabled: true })),
          enabled: true,
        }
    await api.saveProvider(built)
    setOpen(false)
    onSaved()
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button>
          <Plus className="h-4 w-4 mr-2" />
          {s.providers.add}
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{s.providers.add}</DialogTitle>
        </DialogHeader>
        <div className="space-y-4 pt-2">
          <div className="flex items-center gap-3">
            <Checkbox
              id="custom"
              checked={custom}
              onCheckedChange={(v) => setCustom(v === true)}
            />
            <label htmlFor="custom" className="text-sm">
              {s.providers.addCustom}
            </label>
          </div>
          {!custom ? (
            <Select value={presetId} onValueChange={setPresetId}>
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {PRESETS.map((p) => (
                  <SelectItem key={p.id} value={p.id}>
                    {p.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          ) : (
            <>
              <Input placeholder={s.providers.name} value={name} onChange={(e) => setName(e.target.value)} />
              <Input
                placeholder={s.providers.baseUrl}
                value={baseUrl}
                onChange={(e) => setBaseUrl(e.target.value)}
              />
            </>
          )}
          <Input
            type="password"
            placeholder={s.providers.apiKey}
            value={apiKey}
            onChange={(e) => setApiKey(e.target.value)}
          />
          {error && <p className="text-sm text-destructive break-all">{error}</p>}
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={() => setOpen(false)}>
              {s.providers.cancel}
            </Button>
            {error && (
              <Button variant="secondary" onClick={saveAnyway}>
                {s.providers.saveAnyway}
              </Button>
            )}
            <Button onClick={save} disabled={validating}>
              {validating ? s.providers.validating : s.providers.save}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}

function EditProviderDialog({ provider, onSaved }: { provider: Provider; onSaved: () => void }) {
  const s = useStrings()
  const [open, setOpen] = useState(false)
  const [name, setName] = useState(provider.name)
  const [baseUrl, setBaseUrl] = useState(provider.base_url)
  const [apiKey, setApiKey] = useState(provider.api_key ?? '')
  const [validating, setValidating] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const save = async () => {
    const next: Provider = {
      ...provider,
      name: name || provider.name,
      base_url: baseUrl || provider.base_url,
      api_key: apiKey || null,
    }
    setError(null)
    setValidating(true)
    try {
      // Validate the key; merge freshly discovered models, preserving
      // the enabled state of models the user already picked.
      const ids = await api.validateProvider(next)
      const existing = new Map(next.models.map((m) => [m.id, m]))
      next.models = ids.map((id) => existing.get(id) ?? { id, enabled: false })
      await api.saveProvider(next)
      setOpen(false)
      onSaved()
    } catch (e) {
      setError(`${s.providers.validationFailed}: ${String(e)}`)
    } finally {
      setValidating(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="ghost" size="icon" title={s.providers.edit}>
          <Pencil className="h-4 w-4" />
        </Button>
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {s.providers.edit} — {provider.name}
          </DialogTitle>
        </DialogHeader>
        <div className="space-y-4 pt-2">
          <Input placeholder={s.providers.name} value={name} onChange={(e) => setName(e.target.value)} />
          <Input placeholder={s.providers.baseUrl} value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} />
          <Input
            type="password"
            placeholder={s.providers.apiKey}
            value={apiKey}
            onChange={(e) => setApiKey(e.target.value)}
          />
          {error && <p className="text-sm text-destructive break-all">{error}</p>}
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={() => setOpen(false)}>
              {s.providers.cancel}
            </Button>
            <Button onClick={save} disabled={validating}>
              {validating ? s.providers.validating : s.providers.save}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}

function ProviderCard({ provider, onChanged }: { provider: Provider; onChanged: () => void }) {
  const s = useStrings()
  const [busy, setBusy] = useState(false)
  const [discovered, setDiscovered] = useState<string[]>([])
  const [fetchError, setFetchError] = useState<string | null>(null)
  const [query, setQuery] = useState('')
  const enabledCount = provider.models.filter((m) => m.enabled).length

  const discover = async () => {
    setBusy(true)
    setFetchError(null)
    try {
      setDiscovered(await api.discoverModels(provider.id))
    } catch (e) {
      setDiscovered([])
      setFetchError(`${s.providers.discoverFailed}: ${String(e)}`)
    } finally {
      setBusy(false)
    }
  }

  const toggle = async (model: string, enabled: boolean) => {
    await api.toggleModel(provider.id, model, enabled)
    onChanged()
  }

  const known = new Set(provider.models.map((m) => m.id))
  const newModels = discovered.filter((id) => !known.has(id))

  // Aggregators can expose hundreds of models: enabled first, then a
  // substring filter keeps the list navigable.
  const q = query.trim().toLowerCase()
  const sortedModels = [...provider.models].sort(
    (a, b) => Number(b.enabled) - Number(a.enabled) || a.id.localeCompare(b.id),
  )
  const visibleModels = q ? sortedModels.filter((m) => m.id.toLowerCase().includes(q)) : sortedModels
  const visibleNew = q ? newModels.filter((id) => id.toLowerCase().includes(q)) : newModels
  const totalCount = provider.models.length + newModels.length
  const shownCount = visibleModels.length + visibleNew.length

  return (
    <Card>
      <CardHeader className="flex-row items-center justify-between space-y-0">
        <div className="flex items-center gap-3">
          <CardTitle className="text-base">{provider.name}</CardTitle>
          <Badge variant="secondary">{provider.protocol}</Badge>
          <Badge variant="outline">
            {s.providers.enabledModels.replace('{{count}}', String(enabledCount))}
          </Badge>
        </div>
        <div className="flex items-center gap-2">
          <Button variant="outline" size="sm" onClick={discover} disabled={busy}>
            <RefreshCw className={`h-4 w-4 mr-2 ${busy ? 'animate-spin' : ''}`} />
            {busy ? s.providers.discovering : s.providers.discover}
          </Button>
          <EditProviderDialog provider={provider} onSaved={onChanged} />
          <Button
            variant="ghost"
            size="icon"
            onClick={async () => {
              await api.deleteProvider(provider.id)
              onChanged()
            }}
          >
            <Trash2 className="h-4 w-4 text-destructive" />
          </Button>
        </div>
      </CardHeader>
      <CardContent className="space-y-2">
        {fetchError && <p className="text-sm text-destructive break-all">{fetchError}</p>}
        {totalCount > 8 && (
          <div className="flex items-center gap-3 pb-1">
            <Input
              placeholder={s.providers.searchModels}
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              className="max-w-xs"
            />
            {q && (
              <span className="text-xs text-muted-foreground whitespace-nowrap">
                {s.providers.showingCount
                  .replace('{{shown}}', String(shownCount))
                  .replace('{{total}}', String(totalCount))}
              </span>
            )}
          </div>
        )}
        <div className="max-h-96 overflow-y-auto space-y-2 pr-1">
          {visibleModels.map((m) => (
            <label key={m.id} className="flex items-center gap-3 text-sm">
              <Switch checked={m.enabled} onCheckedChange={(v) => toggle(m.id, v)} />
              <span>{m.label ?? m.id}</span>
              <span className="text-xs text-muted-foreground">{m.id}</span>
            </label>
          ))}
          {visibleNew.map((id) => (
            <label key={id} className="flex items-center gap-3 text-sm text-muted-foreground">
              <Switch checked={false} onCheckedChange={(v) => toggle(id, v)} />
              <span>{id}</span>
            </label>
          ))}
        </div>
        {shownCount === 0 && q && (
          <p className="text-sm text-muted-foreground">{s.providers.noMatch}</p>
        )}
        {provider.models.length === 0 && newModels.length === 0 && (
          <p className="text-sm text-muted-foreground">{s.providers.noModels}</p>
        )}
      </CardContent>
    </Card>
  )
}
