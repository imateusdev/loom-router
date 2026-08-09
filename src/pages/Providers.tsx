import { memo, useEffect, useMemo, useState, type ReactNode } from 'react'
import { MoreHorizontal, Pencil, Plus, RefreshCw, Trash2 } from 'lucide-react'
import { api } from '@/lib/api'
import { useBackendState } from '@/lib/events'
import { useStrings } from '@/i18n'
import { formatContextWindow } from '@/lib/utils'
import {
  PRESETS,
  type AppConfig,
  type ClaudeAuthStatus,
  type ContextWindow,
  type Provider,
  type ProviderProtocol,
} from '@/types'
import PageShell, { CARD_GRID } from '@/components/PageShell'
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

function ProviderSkeletonCard() {
  return (
    <Card className="min-w-0 h-full overflow-hidden" aria-hidden>
      <CardHeader>
        <div className="flex items-center gap-3">
          <div className="h-5 w-9 rounded-full bg-muted animate-pulse" />
          <div className="h-4 w-36 rounded bg-muted animate-pulse" />
        </div>
        <div className="flex min-w-0 flex-wrap items-center justify-end gap-2">
          <div className="h-5 w-14 rounded-full bg-muted animate-pulse" />
          <div className="h-5 w-20 rounded-full bg-muted animate-pulse" />
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="h-4 w-full rounded bg-muted animate-pulse" />
        <div className="h-4 w-2/3 rounded bg-muted animate-pulse" />
        <div className="h-4 w-1/2 rounded bg-muted animate-pulse" />
      </CardContent>
    </Card>
  )
}

export default function ProvidersPage() {
  const s = useStrings()
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  // Context windows come from the backend rather than being derived here:
  // they must be the exact figure published to Codex, and the rule that
  // produces it lives in one place (codex::context_window_for).
  const [windows, setWindows] = useState<Record<string, ContextWindow> | null>(null)
  // Login state of the local claude CLI, for the claude-code provider card.
  const [claudeAuth, setClaudeAuth] = useState<ClaudeAuthStatus | null>(null)

  const fetchData = () => {
    // A missing window map only costs a tag, so its failure is not surfaced.
    api.contextWindows().then(setWindows).catch(() => setWindows(null))
    // Same for the claude auth probe: the card just omits the badge.
    api.claudeAuthStatus().then(setClaudeAuth).catch(() => setClaudeAuth(null))
    return api
      .getConfig()
      .then(setConfig)
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false))
  }
  const load = (showLoading: boolean) => {
    if (showLoading) setLoading(true)
    return fetchData()
  }
  const reload = () => load(true)

  useEffect(() => {
    fetchData()
  }, [])
  // Providers can be enabled or disabled from the tray menu.
  useBackendState(reload)

  // P9: optimistic toggle - patch the provider in place instead of
  // refetching the whole config (and re-rendering every card).
  const toggleModel = async (providerId: string, modelId: string, enabled: boolean) => {
    setConfig((prev) => {
      if (!prev) return prev
      const p = prev.providers[providerId]
      if (!p) return prev
      const known = p.models.some((m) => m.id === modelId)
      const models = known
        ? p.models.map((m) => (m.id === modelId ? { ...m, enabled } : m))
        : [...p.models, { id: modelId, enabled, supports_vision: false }]
      return { ...prev, providers: { ...prev.providers, [providerId]: { ...p, models } } }
    })
    try {
      await api.toggleModel(providerId, modelId, enabled)
    } catch {
      // Roll back to backend truth if the toggle failed.
      reload()
    }
  }

  if (error) return <PageShell title={s.providers.title} subtitle={String(error)}>{null}</PageShell>
  if (loading || !config) {
    return (
      <PageShell
        title={s.providers.title}
        subtitle={s.providers.subtitle}
        actions={<AddProviderDialog onSaved={reload} />}
      >
        <div className={CARD_GRID}>
          {Array.from({ length: 3 }, (_, i) => <ProviderSkeletonCard key={i} />)}
        </div>
      </PageShell>
    )
  }

  const providers = Object.values(config.providers)

  return (
    <PageShell
      title={s.providers.title}
      subtitle={s.providers.subtitle}
      actions={<AddProviderDialog onSaved={reload} />}
    >
      {providers.length === 0 ? (
        <Card className="min-w-0 min-h-[200px]">
          <CardContent className="flex flex-1 items-center justify-center">
            <p className="text-sm text-muted-foreground">{s.providers.noProviders}</p>
          </CardContent>
        </Card>
      ) : (
        <div className={CARD_GRID}>
          {providers.map((p) => (
            <ProviderCard
              key={p.id}
              provider={p}
              windows={windows}
              claudeAuth={claudeAuth}
              onToggle={toggleModel}
              onChanged={reload}
            />
          ))}
        </div>
      )}
    </PageShell>
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

  // D1: single builder shared by `save` and `saveAnyway`.
  const buildProviderFromForm = (): Provider => {
    const preset = PRESETS.find((p) => p.id === presetId)!
    return custom
      ? {
          id: name.toLowerCase().replace(/[^a-z0-9]+/g, '-') || 'custom',
          name: name || 'Custom',
          protocol: 'openai',
          base_url: baseUrl,
          api_key: apiKey || null,
          has_key: false,
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
          has_key: false,
          user_agent: preset.userAgent ?? null,
          // A seeded model may name the dialect the gateway serves it in;
          // a bare id just follows the provider's.
          models: (preset.defaultModels ?? []).map((m) =>
            typeof m === 'string'
              ? { id: m, enabled: true, supports_vision: false }
              : { id: m[0], protocol: m[1], enabled: true, supports_vision: false },
          ),
          enabled: true,
        }
  }

  const save = async () => {
    const built = buildProviderFromForm()
    setError(null)
    setValidating(true)
    try {
      // Validate the key and seed the model list in one call.
      const ids = await api.validateProvider(built)
      if (ids.length > 0) {
        const existing = new Map(built.models.map((m) => [m.id, m]))
        built.models = ids.map((id) => existing.get(id) ?? { id, enabled: false, supports_vision: false })
      }
      await api.saveProvider(built)
      // Fill context windows and protocol/vision details as soon as the
      // provider exists. A provider whose /models route is unreachable still
      // saves fine; discovery is best-effort here.
      await api.discoverModels(built.id).catch(() => [])
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
    const built = buildProviderFromForm()
    await api.saveProvider(built)
    await api.discoverModels(built.id).catch(() => [])
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
          {!custom && presetId === 'claude-code' ? (
            <p className="text-xs text-muted-foreground">{s.providers.claudeNoKey}</p>
          ) : (
            <Input
              type="password"
              placeholder={s.providers.apiKey}
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
            />
          )}
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

function EditProviderDialog({
  provider,
  onSaved,
  trigger,
  onOpenChange,
}: {
  provider: Provider
  onSaved: () => void
  trigger?: ReactNode
  onOpenChange?: (open: boolean) => void
}) {
  const s = useStrings()
  const [open, setOpen] = useState(false)
  const [name, setName] = useState(provider.name)
  const [baseUrl, setBaseUrl] = useState(provider.base_url)
  // S4: the backend never sends the real key. Start empty; only send a key
  // when the user typed one, otherwise "" tells the backend to keep it.
  const [apiKey, setApiKey] = useState('')
  const [validating, setValidating] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const save = async () => {
    const next: Provider = {
      ...provider,
      name: name || provider.name,
      base_url: baseUrl || provider.base_url,
      api_key: apiKey,
    }
    setError(null)
    setValidating(true)
    try {
      // Validate the key; merge freshly discovered models, preserving
      // the enabled state of models the user already picked.
      const ids = await api.validateProvider(next)
      const existing = new Map(next.models.map((m) => [m.id, m]))
      next.models = ids.map((id) => existing.get(id) ?? { id, enabled: false, supports_vision: false })
      await api.saveProvider(next)
      await api.discoverModels(next.id).catch(() => [])
      setOpen(false)
      onSaved()
    } catch (e) {
      setError(`${s.providers.validationFailed}: ${String(e)}`)
    } finally {
      setValidating(false)
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(v) => {
        setOpen(v)
        onOpenChange?.(v)
      }}
    >
      <DialogTrigger asChild>
        {trigger ?? (
          <Button variant="ghost" size="icon" title={s.providers.edit}>
            <Pencil className="h-4 w-4" />
          </Button>
        )}
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            {s.providers.edit} - {provider.name}
          </DialogTitle>
        </DialogHeader>
        <div className="space-y-4 pt-2">
          <Input placeholder={s.providers.name} value={name} onChange={(e) => setName(e.target.value)} />
          {provider.id === 'claude-code' ? (
            <p className="text-xs text-muted-foreground">{s.providers.claudeNoKey}</p>
          ) : (
            <>
              <Input placeholder={s.providers.baseUrl} value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} />
              <Input
                type="password"
                placeholder={provider.has_key ? s.providers.apiKeyKeep : s.providers.apiKey}
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
              />
            </>
          )}
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

function DeleteProviderDialog({
  provider,
  onDeleted,
  trigger,
  onOpenChange,
}: {
  provider: Provider
  onDeleted: () => void
  trigger?: ReactNode
  onOpenChange?: (open: boolean) => void
}) {
  const s = useStrings()
  const [open, setOpen] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const confirm = async () => {
    setBusy(true)
    setError(null)
    try {
      await api.deleteProvider(provider.id)
      setOpen(false)
      onDeleted()
    } catch (e) {
      setError(String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(v) => {
        setOpen(v)
        onOpenChange?.(v)
      }}
    >
      <DialogTrigger asChild>
        {trigger ?? (
          <Button variant="ghost" size="icon" title={s.providers.delete}>
            <Trash2 className="h-4 w-4 text-destructive" />
          </Button>
        )}
      </DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{s.providers.deleteTitle}</DialogTitle>
        </DialogHeader>
        <p className="text-sm text-muted-foreground pt-2">
          {s.providers.deleteConfirm.replace('{{name}}', provider.name)}
        </p>
        {error && <p className="text-sm text-destructive break-all pt-2">{error}</p>}
        <div className="flex justify-end gap-2 pt-4">
          <Button variant="outline" onClick={() => setOpen(false)}>
            {s.providers.cancel}
          </Button>
          <Button variant="destructive" onClick={confirm} disabled={busy}>
            {s.providers.delete}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}

/// Context window as a tag: "1M", "256K", "128K".
///
/// A window LoomRouter only guessed at is shown muted and marked, because
/// the number is a conservative fallback rather than the model's real limit
/// - presenting it plainly would make every unconfigured provider look like
/// a 128k model.
const PROTOCOLS: ProviderProtocol[] = ['openai', 'anthropic', 'responses']

/// Every dialect a provider actually speaks: its own, plus any a model
/// overrides it with. One entry for an ordinary endpoint, three for a
/// gateway like OpenCode.
function dialectsInUse(provider: Provider): ProviderProtocol[] {
  const seen = new Set<ProviderProtocol>([provider.protocol])
  for (const m of provider.models) if (m.protocol) seen.add(m.protocol)
  return PROTOCOLS.filter((p) => seen.has(p))
}

/// The protocol is detected by a real upstream probe when models are fetched
/// or enabled. Show the result, not a choice the user cannot verify.
function DetectedDialect({
  provider,
  model,
}: {
  provider: Provider
  model: { id: string; protocol?: ProviderProtocol | null }
}) {
  const s = useStrings()
  return (
    <Badge
      variant="outline"
      className="shrink-0 px-2 py-0 text-xs font-normal"
      title={s.providers.modelDialectHint}
    >
      {model.protocol ?? provider.protocol}
    </Badge>
  )
}

function ContextWindowTag({ info }: { info?: ContextWindow }) {
  const s = useStrings()
  if (!info) return null
  const t = formatContextWindow(info.window)
  return (
    <span
      title={info.known ? s.providers.contextKnown : s.providers.contextGuess}
      className={
        'ml-auto shrink-0 rounded-full border px-2 py-0.5 font-mono text-[11px] leading-none ' +
        (info.known
          ? 'border-border text-muted-foreground'
          : 'border-dashed border-border text-muted-foreground/60')
      }
    >
      {t}
      {!info.known && <span className="ml-1 not-italic">?</span>}
    </span>
  )
}

function ProviderActionsMenu({
  provider,
  onChanged,
}: {
  provider: Provider
  onChanged: () => void
}) {
  const s = useStrings()
  const [open, setOpen] = useState(false)
  return (
    <div className="relative">
      <Button variant="ghost" size="icon" title={s.providers.moreActions} onClick={() => setOpen((v) => !v)}>
        <MoreHorizontal className="h-4 w-4" />
      </Button>
      {open && (
        <>
          <div className="fixed inset-0 z-10" onClick={() => setOpen(false)} />
          <div className="absolute right-0 z-20 mt-1 w-44 rounded-md border bg-popover p-1 shadow-md">
            <EditProviderDialog
              provider={provider}
              onSaved={onChanged}
              onOpenChange={(v) => {
                if (!v) setOpen(false)
              }}
              trigger={
                <Button variant="ghost" size="sm" className="w-full justify-start">
                  <Pencil className="h-4 w-4" />
                  {s.providers.edit}
                </Button>
              }
            />
            <DeleteProviderDialog
              provider={provider}
              onDeleted={onChanged}
              onOpenChange={(v) => {
                if (!v) setOpen(false)
              }}
              trigger={
                <Button variant="ghost" size="sm" className="w-full justify-start text-destructive">
                  <Trash2 className="h-4 w-4" />
                  {s.providers.delete}
                </Button>
              }
            />
          </div>
        </>
      )}
    </div>
  )
}

const ProviderCard = memo(function ProviderCard({
  provider,
  windows,
  claudeAuth,
  onToggle,
  onChanged,
}: {
  provider: Provider
  windows: Record<string, ContextWindow> | null
  claudeAuth: ClaudeAuthStatus | null
  onToggle: (providerId: string, modelId: string, enabled: boolean) => void
  onChanged: () => void
}) {
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
      // Discovery also learns context windows backend-side; reload so the
      // tags reflect them instead of the stale pre-fetch guesses.
      onChanged()
    } catch (e) {
      setDiscovered([])
      setFetchError(`${s.providers.discoverFailed}: ${String(e)}`)
    } finally {
      setBusy(false)
    }
  }

  // Only a gateway serving several dialects needs a per-model picker.
  const multiDialect = dialectsInUse(provider).length > 1

  // Aggregators can expose hundreds of models: enabled first, then a
  // substring filter keeps the list navigable. Memoized so typing in the
  // filter doesn't re-sort the whole array on every keystroke.
  const { visibleModels, visibleNew, totalCount, shownCount, q } = useMemo(() => {
    const known = new Set(provider.models.map((m) => m.id))
    const newModels = discovered.filter((id) => !known.has(id))
    const q = query.trim().toLowerCase()
    const sortedModels = [...provider.models].sort(
      (a, b) => Number(b.enabled) - Number(a.enabled) || a.id.localeCompare(b.id),
    )
    const visibleModels = q
      ? sortedModels.filter((m) => m.id.toLowerCase().includes(q))
      : sortedModels
    const visibleNew = q ? newModels.filter((id) => id.toLowerCase().includes(q)) : newModels
    return {
      visibleModels,
      visibleNew,
      totalCount: provider.models.length + newModels.length,
      shownCount: visibleModels.length + visibleNew.length,
      q,
    }
  }, [provider.models, discovered, query])

  return (
    <Card className="min-w-0 h-full">
      {/* Native card-header grid: the title owns the first row and the badges
          the second, so a long name ("Claude Code") can never
          push a badge past the card edge. The actions sit in the right-hand
          `auto` column, spanning both rows. */}
      <CardHeader>
        <div className="space-y-3">
          <div className="flex min-w-0 flex-wrap items-center gap-3">
            <Switch
              checked={provider.enabled}
              onCheckedChange={async (enabled) => {
                await api.setProviderEnabled(provider.id, enabled)
                onChanged()
              }}
              aria-label={s.providers.providerEnabled}
            />
            <div className="min-w-0 flex-1">
              <CardTitle className="truncate text-base" title={provider.name}>{provider.name}</CardTitle>
              <p className="mt-0.5 text-[10px] font-mono uppercase tracking-wide text-muted-foreground">
                {provider.id === 'claude-code' ? s.providers.modePlan : s.providers.modeApi}
              </p>
            </div>
            <div className="ml-auto flex shrink-0 items-center gap-2">
              <Button variant="outline" size="sm" onClick={discover} disabled={busy}>
                <RefreshCw className={`h-4 w-4 mr-2 ${busy ? 'animate-spin' : ''}`} />
                {busy ? s.providers.discovering : s.providers.discover}
              </Button>
              <ProviderActionsMenu provider={provider} onChanged={onChanged} />
            </div>
          </div>
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            {dialectsInUse(provider).map((protocol) => (
              <Badge key={protocol} variant="secondary">
                {protocol}
              </Badge>
            ))}
            {provider.id === 'claude-code' && claudeAuth && (
              claudeAuth.logged_in ? (
                <Badge
                  variant="secondary"
                  className="text-emerald-700 dark:text-emerald-400"
                  title={`${claudeAuth.email ?? ''} · ${claudeAuth.auth_method ?? ''}`}
                >
                  {s.providers.claudePlan.replace(
                    '{{plan}}',
                    claudeAuth.plan ?? claudeAuth.subscription_type ?? '',
                  )}
                </Badge>
              ) : (
                <Badge variant="destructive" title={claudeAuth.error ?? undefined}>
                  {claudeAuth.error?.includes('not found') || claudeAuth.error?.includes('não encontrado')
                    ? s.providers.claudeCliMissing
                    : s.providers.claudeNotLoggedIn}
                </Badge>
              )
            )}
            <Badge variant="outline">
              {s.providers.enabledModels.replace('{{count}}', String(enabledCount))}
            </Badge>
          </div>
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
            <label key={m.id} className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1.5 text-sm">
              <Switch checked={m.enabled} onCheckedChange={(v) => onToggle(provider.id, m.id, v)} />
              {/* Titles because these truncate once the grid runs three
                  columns wide, and a half-shown model id is unusable. */}
              <span className="min-w-0 flex-1 truncate" title={m.label ?? m.id}>
                {m.label ?? m.id}
              </span>
              {/* The upstream id is only worth a second column when it
                  differs from what is displayed; `label ?? id` otherwise
                  printed the same name twice. */}
              {m.label && m.label !== m.id && (
                <span className="min-w-0 truncate font-mono text-xs text-muted-foreground" title={m.id}>
                  {m.id}
                </span>
              )}
              {m.fast_mode && (
                <Badge
                  variant="outline"
                  className="shrink-0 px-1.5 py-0 text-[10px] leading-none"
                  title={s.providers.fastMode}
                >
                  {s.providers.fastMode}
                </Badge>
              )}
              <ContextWindowTag info={windows?.[`${provider.id}/${m.id}`]} />
              {multiDialect && (
                <DetectedDialect provider={provider} model={m} />
              )}
            </label>
          ))}
          {visibleNew.map((id) => (
            <label
              key={id}
              className="flex min-w-0 items-center gap-3 text-sm text-muted-foreground"
            >
              <Switch checked={false} onCheckedChange={(v) => onToggle(provider.id, id, v)} />
              {/* Discovered ids come straight from aggregator catalogues and
                  are the long case ("meta-llama/llama-3.1-405b-instruct:free"),
                  in a card that is 340px once the grid goes two-up. */}
              <span className="min-w-0 flex-1 truncate" title={id}>
                {id}
              </span>
            </label>
          ))}
        </div>
        {shownCount === 0 && q && (
          <p className="text-sm text-muted-foreground">{s.providers.noMatch}</p>
        )}
        {provider.models.length === 0 && visibleNew.length === 0 && (
          <p className="text-sm text-muted-foreground">{s.providers.noModels}</p>
        )}
      </CardContent>
    </Card>
  )
})
