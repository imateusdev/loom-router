import { useEffect, useState } from 'react'
import { Plus, RefreshCw, Trash2 } from 'lucide-react'
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
          <p className="text-sm text-muted-foreground">{s.providers.noModels}</p>
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

  const save = async () => {
    const preset = PRESETS.find((p) => p.id === presetId)!
    const built: Provider = custom
      ? {
          id: name.toLowerCase().replace(/[^a-z0-9]+/g, '-') || 'custom',
          name: name || 'Custom',
          protocol: 'openai',
          base_url: baseUrl,
          api_key: apiKey || null,
          models: [],
          enabled: true,
        }
      : {
          id: preset.id,
          name: preset.name,
          protocol: preset.protocol,
          base_url: preset.base_url,
          api_key: apiKey || null,
          models: [],
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
          <div className="flex justify-end gap-2">
            <Button variant="outline" onClick={() => setOpen(false)}>
              {s.providers.cancel}
            </Button>
            <Button onClick={save}>{s.providers.save}</Button>
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
  const enabledCount = provider.models.filter((m) => m.enabled).length

  const discover = async () => {
    setBusy(true)
    try {
      setDiscovered(await api.discoverModels(provider.id))
    } catch (e) {
      setDiscovered([])
      console.error(e)
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
        {provider.models.map((m) => (
          <label key={m.id} className="flex items-center gap-3 text-sm">
            <Switch checked={m.enabled} onCheckedChange={(v) => toggle(m.id, v)} />
            <span>{m.label ?? m.id}</span>
            <span className="text-xs text-muted-foreground">{m.id}</span>
          </label>
        ))}
        {newModels.map((id) => (
          <label key={id} className="flex items-center gap-3 text-sm text-muted-foreground">
            <Switch checked={false} onCheckedChange={(v) => toggle(id, v)} />
            <span>{id}</span>
          </label>
        ))}
        {provider.models.length === 0 && newModels.length === 0 && (
          <p className="text-sm text-muted-foreground">{s.providers.noModels}</p>
        )}
      </CardContent>
    </Card>
  )
}
