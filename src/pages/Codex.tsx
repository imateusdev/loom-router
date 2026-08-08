import { useEffect, useState } from 'react'
import { CheckCircle2, XCircle } from 'lucide-react'
import { api } from '@/lib/api'
import { useBackendState } from '@/lib/events'
import { useStrings } from '@/i18n'
import type { AppConfig, CodexStatus, VisualAssistanceConfig } from '@/types'
import PageShell from '@/components/PageShell'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'

// Radix Select forbids empty-string item values, so "off" (null) is carried
// by a sentinel on the form side.
const OFF_SENTINEL = '__off__'
const EMPTY_VISUAL_ASSISTANCE: VisualAssistanceConfig = {
  enabled: false,
  assistant_model: null,
  fallback_models: [],
}

function CodexSkeletonCard({ wide = false }: { wide?: boolean }) {
  return (
    <Card className={wide ? 'mb-6 min-w-0' : 'min-w-0'}>
      <CardHeader>
        <div className="h-4 w-32 rounded bg-muted animate-pulse" />
      </CardHeader>
      <CardContent className="flex flex-1 flex-col space-y-3">
        <div className="h-4 w-full rounded bg-muted animate-pulse" />
        <div className="h-4 w-2/3 rounded bg-muted animate-pulse" />
        <div className="h-4 w-1/2 rounded bg-muted animate-pulse" />
      </CardContent>
    </Card>
  )
}

export default function CodexPage() {
  const s = useStrings()
  const [status, setStatus] = useState<CodexStatus | null>(null)
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [nativeModels, setNativeModels] = useState<string[]>([])
  const [busy, setBusy] = useState(false)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const fetchData = () => {
    return Promise.all([api.getConfig(), api.codexStatus(), api.codexNativeModels()])
      .then(([cfg, st, native]) => {
        setConfig(cfg)
        setStatus(st)
        setNativeModels(native)
      })
      .catch((e) => setError(String(e instanceof Error ? e.message : e)))
      .finally(() => setLoading(false))
  }
  const reload = () => fetchData()
  useEffect(() => {
    fetchData()
  }, [])
  // The tray applies and removes the integration too.
  useBackendState(reload)

  const apply = async () => {
    setBusy(true)
    setError(null)
    try {
      await api.codexApply()
      await reload()
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e))
    } finally {
      setBusy(false)
    }
  }

  const remove = async () => {
    setBusy(true)
    setError(null)
    try {
      await api.codexRemove()
      await reload()
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e))
    } finally {
      setBusy(false)
    }
  }

  const active = status?.managed_block_present ?? false

  if (loading) {
    return (
      <PageShell title={s.codex.title} subtitle={s.codex.subtitle}>
        <CodexSkeletonCard wide />
        <div className="grid grid-cols-[repeat(auto-fit,minmax(240px,1fr))] items-stretch gap-6">
          <CodexSkeletonCard />
          <CodexSkeletonCard />
          <CodexSkeletonCard />
        </div>
      </PageShell>
    )
  }

  return (
    <PageShell title={s.codex.title} subtitle={s.codex.subtitle}>
      <Card className="mb-6">
        <CardHeader className="flex-row items-center justify-between space-y-0">
          <CardTitle className="text-base">Codex</CardTitle>
          <Badge variant={active ? 'default' : 'secondary'}>
            {active ? s.codex.applied : s.codex.notApplied}
          </Badge>
        </CardHeader>
        <CardContent className="space-y-4">
          <StatusRow ok={status?.config_exists ?? false} label={s.codex.codexHome} detail={status?.codex_home} />
          {/* A red row with no explanation is where this bug stranded people:
              say what to do instead of just failing. */}
          <StatusRow
            ok={status?.codex_cli_available ?? false}
            label={s.codex.cliAvailable}
            detail={status?.codex_cli_available ? undefined : s.codex.cliMissingHint}
          />
          <StatusRow ok={status?.native_catalog_present ?? false} label={s.codex.nativeCatalog} />
          <StatusRow
            ok={status?.merged_catalog_present ?? false}
            label={s.codex.mergedCatalog}
            detail={s.codex.modelsInPicker.replace('{{count}}', String(status?.merged_model_count ?? 0))}
          />
          <div className="flex gap-2">
            {active ? (
              <Button variant="outline" onClick={remove} disabled={busy}>
                {s.codex.remove}
              </Button>
            ) : (
              <Button onClick={apply} disabled={busy}>
                {s.codex.apply}
              </Button>
            )}
          </div>
          {status?.managed_block_orphaned && (
            <p className="text-xs text-amber-600 dark:text-amber-500">{s.codex.orphanedHint}</p>
          )}
          {error && <p className="text-xs text-red-600 dark:text-red-500">{error}</p>}
          <p className="text-xs text-muted-foreground">{s.codex.restartHint}</p>
          {status?.integration_enabled && (
            <p className="text-xs text-green-600 dark:text-green-500">{s.codex.autoApplyHint}</p>
          )}
        </CardContent>
      </Card>

      {/* A fixed trio, so a tighter track than the shared CARD_GRID: at 340px
          the third card was orphaned on its own row from 992px through 1371px,
          which includes the default 1100x760 window. */}
      <div className="grid grid-cols-[repeat(auto-fit,minmax(240px,1fr))] items-stretch gap-6">
        <ActiveModelCard config={config} nativeModels={nativeModels} onChanged={setConfig} />
        <SideCallCard config={config} nativeModels={nativeModels} onChanged={setConfig} />
        <VisualAssistanceCard config={config} onChanged={setConfig} />
        <NativeSlugCard config={config} onChanged={setConfig} onReload={reload} />
        <MultiAgentCard />
      </div>
    </PageShell>
  )
}

function VisualAssistanceCard({
  config,
  onChanged,
}: {
  config: AppConfig | null
  onChanged: (config: AppConfig) => void
}) {
  const s = useStrings()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [fallbackCandidate, setFallbackCandidate] = useState(OFF_SENTINEL)
  const assistance = config?.visual_assistance ?? EMPTY_VISUAL_ASSISTANCE

  const visionModels = config
    ? Object.values(config.providers).flatMap((provider) =>
        provider.enabled && provider.has_key
          ? provider.models
              .filter(
                (model) =>
                  model.supports_vision &&
                  (model.protocol ?? provider.protocol) !== 'responses',
              )
              .map((model) => ({ slug: `${provider.id}/${model.id}`, label: model.label ?? model.id }))
          : [],
      )
    : []
  const supportsVision = (slug: string | null) =>
    slug !== null && visionModels.some((model) => model.slug === slug)
  const fallbackOptions = visionModels.filter(
    (model) => model.slug !== assistance.assistant_model && !assistance.fallback_models.includes(model.slug),
  )

  const save = async (next: VisualAssistanceConfig) => {
    // A config can arrive from an older build or a concurrent edit. Normalize
    // it at the write boundary so the primary cannot also become a fallback
    // and duplicate fallback slugs do not leak into persisted routing order.
    const normalized: VisualAssistanceConfig = {
      ...next,
      fallback_models: [...new Set(next.fallback_models)].filter(
        (model) => model !== next.assistant_model,
      ),
    }
    const invalidSelection =
      (normalized.assistant_model !== null && !supportsVision(normalized.assistant_model)) ||
      normalized.fallback_models.some((model) => !supportsVision(model))
    if (normalized.enabled && normalized.assistant_model === null) {
      setError(s.codex.visualAssistancePrimaryRequired)
      return false
    }
    if (invalidSelection) {
      setError(s.codex.visualAssistanceInvalidModel)
      return false
    }

    setBusy(true)
    setError(null)
    try {
      await api.setVisualAssistance(normalized)
      if (config) onChanged({ ...config, visual_assistance: normalized })
      return true
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e))
      return false
    } finally {
      setBusy(false)
    }
  }

  const addFallback = async () => {
    if (
      fallbackCandidate === OFF_SENTINEL ||
      fallbackCandidate === assistance.assistant_model ||
      assistance.fallback_models.includes(fallbackCandidate) ||
      !supportsVision(fallbackCandidate)
    ) {
      return
    }
    if (await save({ ...assistance, fallback_models: [...assistance.fallback_models, fallbackCandidate] })) {
      setFallbackCandidate(OFF_SENTINEL)
    }
  }

  const moveFallback = (index: number, direction: -1 | 1) => {
    const nextIndex = index + direction
    if (nextIndex < 0 || nextIndex >= assistance.fallback_models.length) return
    const fallbacks = [...assistance.fallback_models]
    ;[fallbacks[index], fallbacks[nextIndex]] = [fallbacks[nextIndex], fallbacks[index]]
    void save({ ...assistance, fallback_models: fallbacks })
  }

  const assistantValue =
    assistance.assistant_model && supportsVision(assistance.assistant_model)
      ? assistance.assistant_model
      : OFF_SENTINEL

  const defaultAssistant = visionModels[0]?.slug ?? null

  return (
    <Card className="min-w-0 h-full">
      <CardHeader>
        <CardTitle className="text-base">{s.codex.visualAssistanceTitle}</CardTitle>
      </CardHeader>
      <CardContent className="flex flex-1 flex-col space-y-3">
        <label className="flex items-center gap-3 text-sm">
          <Switch
            checked={assistance.enabled}
            onCheckedChange={(enabled) =>
              void save({
                ...assistance,
                enabled,
                // Selecting a primary model must not be a prerequisite for
                // discovering the feature. When the user turns assistance on
                // for the first time, use the first eligible visual model as
                // the initial primary; they can change it immediately below.
                assistant_model: enabled ? assistance.assistant_model ?? defaultAssistant : assistance.assistant_model,
              })
            }
            disabled={busy || !config}
            aria-label={s.codex.visualAssistanceTitle}
          />
          <span>{assistance.enabled ? s.common.on : s.common.off}</span>
        </label>

        <Select
          value={assistantValue}
          onValueChange={(value) => {
            setFallbackCandidate(OFF_SENTINEL)
            void save({ ...assistance, assistant_model: value === OFF_SENTINEL ? null : value })
          }}
          disabled={busy || !config}
        >
          <SelectTrigger aria-label={s.codex.visualAssistancePrimary}>
            <SelectValue placeholder={s.codex.visualAssistancePrimary} />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={OFF_SENTINEL}>{s.codex.visualAssistancePrimaryOff}</SelectItem>
            {visionModels.map((model) => (
              <SelectItem key={model.slug} value={model.slug}>
                {model.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>

        <div className="space-y-2">
          <div className="flex flex-col gap-2">
            <Select
              value={fallbackCandidate}
              onValueChange={setFallbackCandidate}
              disabled={busy || !config || !assistance.enabled || fallbackOptions.length === 0}
            >
              <SelectTrigger className="w-full" aria-label={s.codex.visualAssistanceFallback}>
                <SelectValue placeholder={s.codex.visualAssistanceFallbackPlaceholder} />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={OFF_SENTINEL}>{s.codex.visualAssistanceFallbackPlaceholder}</SelectItem>
                {fallbackOptions.map((model) => (
                  <SelectItem key={model.slug} value={model.slug}>
                    {model.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button
              variant="outline"
              className="w-full"
              onClick={() => void addFallback()}
              disabled={busy || !assistance.enabled || fallbackCandidate === OFF_SENTINEL}
            >
              {s.codex.visualAssistanceAddFallback}
            </Button>
          </div>
          {assistance.fallback_models.length === 0 ? (
            <p className="text-xs text-muted-foreground">{s.codex.visualAssistanceNoFallbacks}</p>
          ) : (
            <ol className="space-y-1">
              {assistance.fallback_models.map((model, index) => {
                const label = visionModels.find((option) => option.slug === model)?.label ?? model
                return (
                  <li key={model} className="flex items-center justify-between gap-2 text-sm">
                    <span>{label}</span>
                    <span className="flex gap-1">
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => moveFallback(index, -1)}
                        disabled={busy || !assistance.enabled || index === 0}
                        aria-label={s.codex.visualAssistanceMoveUp.replace('{{model}}', label)}
                      >
                        ↑
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => moveFallback(index, 1)}
                        disabled={busy || !assistance.enabled || index === assistance.fallback_models.length - 1}
                        aria-label={s.codex.visualAssistanceMoveDown.replace('{{model}}', label)}
                      >
                        ↓
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() =>
                          void save({
                            ...assistance,
                            fallback_models: assistance.fallback_models.filter((_, i) => i !== index),
                          })
                        }
                        disabled={busy || !assistance.enabled}
                        aria-label={s.codex.visualAssistanceRemove.replace('{{model}}', label)}
                      >
                        ×
                      </Button>
                    </span>
                  </li>
                )
              })}
            </ol>
          )}
        </div>
        {error && <p className="text-xs text-red-600 dark:text-red-500">{error}</p>}
        <p className="mt-auto text-xs text-muted-foreground">{s.codex.visualAssistanceDescription}</p>
      </CardContent>
    </Card>
  )
}

// The model Codex opens new sessions with. Mirrors the menu-bar picker:
// a switch that only exists in the tray is a setting the window cannot
// explain or undo.
function ActiveModelCard({
  config,
  nativeModels,
  onChanged,
}: {
  config: AppConfig | null
  nativeModels: string[]
  onChanged: (config: AppConfig) => void
}) {
  const s = useStrings()
  const [busy, setBusy] = useState(false)
  const value = config?.active_model ?? OFF_SENTINEL

  const models = config
    ? [
        ...nativeModels,
        ...Object.values(config.providers)
          .filter((p) => p.enabled)
          .flatMap((p) => p.models.filter((m) => m.enabled).map((m) => `${p.id}/${m.id}`)),
      ]
    : nativeModels
  // A previously picked model that was since disabled stays selectable, so
  // the field shows what is stored rather than silently reading as "off".
  const options = models.includes(value) || value === OFF_SENTINEL ? models : [value, ...models]

  const change = async (next: string) => {
    const slug = next === OFF_SENTINEL ? null : next
    setBusy(true)
    try {
      await api.setActiveModel(slug)
      if (config) onChanged({ ...config, active_model: slug })
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card className="min-w-0 h-full">
      <CardHeader>
        <CardTitle className="text-base">{s.codex.activeModelTitle}</CardTitle>
      </CardHeader>
      <CardContent className="flex flex-1 flex-col space-y-3">
        <Select value={value} onValueChange={change} disabled={busy || !config}>
          <SelectTrigger>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={OFF_SENTINEL}>{s.codex.activeModelOff}</SelectItem>
            {options.map((m) => (
              <SelectItem key={m} value={m}>
                {m}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <p className="text-xs text-muted-foreground">{s.codex.activeModelRestart}</p>
        <p className="mt-auto text-xs text-muted-foreground">{s.codex.activeModelDescription}</p>
      </CardContent>
    </Card>
  )
}

function SideCallCard({
  config,
  nativeModels,
  onChanged,
}: {
  config: AppConfig | null
  nativeModels: string[]
  onChanged: (config: AppConfig) => void
}) {
  const s = useStrings()
  const [busy, setBusy] = useState(false)
  const value = config?.side_call_fallback ?? OFF_SENTINEL

  // Enabled models across all providers, as "provider/model" slugs.
  const models = config
    ? [
        ...nativeModels,
        ...Object.values(config.providers).flatMap((p) =>
          p.models.filter((m) => m.enabled).map((m) => `${p.id}/${m.id}`),
        ),
      ]
    : nativeModels
  // Keep a previously saved slug selectable even if the model was since disabled.
  const options = models.includes(value) || value === OFF_SENTINEL ? models : [value, ...models]

  const change = async (next: string) => {
    const model = next === OFF_SENTINEL ? null : next
    setBusy(true)
    try {
      await api.setSideCallFallback(model)
      if (config) onChanged({ ...config, side_call_fallback: model })
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card className="min-w-0 h-full">
      <CardHeader>
        <CardTitle className="text-base">{s.codex.sideCallTitle}</CardTitle>
      </CardHeader>
      <CardContent className="flex flex-1 flex-col space-y-3">
        <Select value={value} onValueChange={change} disabled={busy || !config}>
          <SelectTrigger>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={OFF_SENTINEL}>{s.codex.sideCallOff}</SelectItem>
            {options.map((m) => (
              <SelectItem key={m} value={m}>
                {m}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <p className="mt-auto text-xs text-muted-foreground">{s.codex.sideCallDescription}</p>
      </CardContent>
    </Card>
  )
}

function NativeSlugCard({
  config,
  onChanged,
  onReload,
}: {
  config: AppConfig | null
  onChanged: (config: AppConfig) => void
  onReload: () => void
}) {
  const s = useStrings()
  const [busy, setBusy] = useState(false)
  const enabled = config?.native_slug_mode ?? false

  const change = async (next: boolean) => {
    setBusy(true)
    try {
      await api.setNativeSlugMode(next)
      if (config) onChanged({ ...config, native_slug_mode: next })
      // The backend re-applies the integration if it is active; refresh status.
      onReload()
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card className="min-w-0 h-full">
      <CardHeader>
        <CardTitle className="text-base">{s.codex.nativeSlugTitle}</CardTitle>
      </CardHeader>
      <CardContent className="flex flex-1 flex-col space-y-3">
        <label className="flex items-center gap-3 text-sm">
          <Switch checked={enabled} onCheckedChange={change} disabled={busy || !config} />
          <span>{enabled ? s.common.on : s.common.off}</span>
        </label>
        <p className="mt-auto text-xs text-muted-foreground">{s.codex.nativeSlugDescription}</p>
      </CardContent>
    </Card>
  )
}

/// Canonical on/off for Codex's `features.multi_agent`.
///
/// The Agents page shows a banner when this is off, but a banner that only
/// exists in the off state is a one-way door: enabling it makes the only
/// control disappear. The switch lives here, with the other settings written
/// to ~/.codex/config.toml, so the state is always visible and reversible.
function MultiAgentCard() {
  const s = useStrings()
  const [enabled, setEnabled] = useState<boolean | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    let cancelled = false
    api
      .multiAgentStatus()
      .then((v) => {
        if (!cancelled) setEnabled(v)
      })
      .catch(() => {
        if (!cancelled) setEnabled(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  const change = async (next: boolean) => {
    setBusy(true)
    try {
      // The backend returns the state it actually wrote; trust that over the
      // requested value so a failed edit cannot leave the switch lying.
      setEnabled(await api.setMultiAgent(next))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card className="min-w-0 h-full">
      <CardHeader>
        <CardTitle className="text-base">{s.codex.multiAgentTitle}</CardTitle>
      </CardHeader>
      <CardContent className="flex flex-1 flex-col space-y-3">
        <label className="flex items-center gap-3 text-sm">
          <Switch
            checked={enabled ?? false}
            onCheckedChange={change}
            disabled={busy || enabled === null}
          />
          <span>{enabled ? s.common.on : s.common.off}</span>
        </label>
        <p className="mt-auto text-xs text-muted-foreground">{s.codex.multiAgentDescription}</p>
      </CardContent>
    </Card>
  )
}

function StatusRow({ ok, label, detail }: { ok: boolean; label: string; detail?: string }) {
  return (
    <div className="flex items-start gap-2 text-sm">
      {ok ? (
        <CheckCircle2 className="h-4 w-4 mt-0.5 text-green-500" />
      ) : (
        <XCircle className="h-4 w-4 mt-0.5 text-muted-foreground" />
      )}
      <div>
        <div>{label}</div>
        {detail && <div className="text-xs text-muted-foreground font-mono break-all">{detail}</div>}
      </div>
    </div>
  )
}
