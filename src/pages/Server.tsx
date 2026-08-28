import { useCallback, useEffect, useMemo, useState } from 'react'
import { Activity, AlertTriangle, Boxes, CheckCircle2, Play, ShieldCheck, Square, XCircle } from 'lucide-react'
import { api } from '@/lib/api'
import { useBackendState } from '@/lib/events'
import { useStrings } from '@/i18n'
import type { AppConfig, CodexStatus, ServerStatus, SleepPreventionMode, StatsSummary } from '@/types'
import PageShell from '@/components/PageShell'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'

const DAY_SECS = 86_400

function fmt(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 10_000) return `${(n / 1_000).toFixed(1)}K`
  if (n >= 1_000) return `${(n / 1_000).toFixed(2)}K`
  return String(n)
}

function fmtLatency(ms: number | null): string {
  if (ms == null) return '-'
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${ms}ms`
}

function globalLatency(stats: StatsSummary | null): string {
  if (!stats) return '-'
  let weighted = 0
  let requests = 0
  for (const provider of stats.per_provider) {
    for (const model of provider.models) {
      if (model.avg_latency_ms != null) {
        weighted += model.avg_latency_ms * model.requests
        requests += model.requests
      }
    }
  }
  if (requests === 0) return '-'
  return fmtLatency(Math.round(weighted / requests))
}

function totalErrors(stats: StatsSummary | null): number {
  if (!stats) return 0
  return stats.per_provider.reduce(
    (acc, provider) => acc + provider.models.reduce((sum, model) => sum + model.errors, 0),
    0,
  )
}

function providerLatency(stats: StatsSummary | null, providerId: string): string {
  if (!stats) return '-'
  const provider = stats.per_provider.find((item) => item.provider === providerId)
  if (!provider) return '-'
  let weighted = 0
  let requests = 0
  for (const model of provider.models) {
    if (model.avg_latency_ms != null) {
      weighted += model.avg_latency_ms * model.requests
      requests += model.requests
    }
  }
  if (requests === 0) return '-'
  return fmtLatency(Math.round(weighted / requests))
}

function providerName(config: AppConfig | null, providerId: string): string {
  const provider = config?.providers[providerId]
  return provider?.name ?? providerId
}

function Stat({ label, value, tone }: { label: string; value: string; tone?: 'bad' | 'good' }) {
  return (
    <div className="min-w-0 shrink-0">
      <dt className="text-[11px] leading-none text-muted-foreground">{label}</dt>
      <dd
        className={
          'mt-1 font-mono text-sm tabular-nums ' +
          (tone === 'bad'
            ? 'text-destructive'
            : tone === 'good'
              ? 'text-emerald-700 dark:text-emerald-400'
              : '')
        }
      >
        {value}
      </dd>
    </div>
  )
}

export default function ServerPage() {
  const s = useStrings()
  const [status, setStatus] = useState<ServerStatus | null>(null)
  const [stats, setStats] = useState<StatsSummary | null>(null)
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [codex, setCodex] = useState<CodexStatus | null>(null)

  const reload = useCallback(() => {
    Promise.all([
      api.serverStatus(),
      api.statsSummary(DAY_SECS),
      api.getConfig(),
      api.codexStatus(),
    ])
      .then(([serverStatus, summary, cfg, codexStatus]) => {
        setStatus(serverStatus)
        setStats(summary)
        setConfig(cfg)
        setCodex(codexStatus)
      })
      .catch(() => {
        // A failed read should not trap the page in a half-rendered state;
        // the existing values stay put and the next state change retries.
      })
  }, [])
  useEffect(() => {
    reload()
  }, [reload])
  // The tray can start or stop the proxy while this page is open.
  useBackendState(reload)

  const start = async () => setStatus(await api.serverStart())
  const stop = async () => setStatus(await api.serverStop())
  const sleepPrevention = config?.sleep_prevention ?? 'while_active'
  const setSleepPrevention = async (mode: SleepPreventionMode) => {
    await api.setSleepPrevention(mode)
    setConfig((current) => current ? { ...current, sleep_prevention: mode } : current)
  }

  const enabledProviders = useMemo(
    () => Object.values(config?.providers ?? {}).filter((provider) => provider.enabled).length,
    [config],
  )
  const enabledModels = useMemo(
    () =>
      Object.values(config?.providers ?? {}).reduce(
        (count, provider) => count + provider.models.filter((model) => model.enabled).length,
        0,
      ),
    [config],
  )
  const trafficTiles = [
    { label: s.overview.requests, value: fmt(stats?.requests ?? 0) },
    { label: s.overview.avgLatency, value: globalLatency(stats) },
    {
      label: s.overview.failures,
      value: String(totalErrors(stats)),
      tone: totalErrors(stats) > 0 ? ('bad' as const) : undefined,
    },
    { label: s.overview.cacheRatio, value: `${Math.round((stats?.cache_ratio ?? 0) * 100)}%` },
    { label: s.overview.estCost, value: stats?.cost_usd == null ? '-' : `$${stats.cost_usd.toFixed(2)}`, tone: 'good' as const },
  ]

  const providerRows = useMemo(
    () =>
      (stats?.per_provider ?? []).map((provider) => ({
        id: provider.provider,
        name: providerName(config, provider.provider),
        requests: provider.requests,
        share:
          (stats?.requests ?? 0) > 0
            ? `${Math.round((provider.requests / (stats?.requests ?? 1)) * 100)}%`
            : '-',
        errors: provider.models.reduce((sum, model) => sum + model.errors, 0),
        latency: providerLatency(stats, provider.provider),
        cost: provider.cost_usd == null ? '-' : `$${provider.cost_usd.toFixed(2)}`,
      })),
    [config, stats],
  )
  const hasProviderTraffic = (stats?.per_provider.length ?? 0) > 0
  const attentionItems: string[] = []
  if (totalErrors(stats) > 0) {
    attentionItems.push(s.server.attentionErrors.replace('{{count}}', String(totalErrors(stats))))
  }
  if (enabledProviders === 0) {
    attentionItems.push(s.overview.noProviders)
  } else if (!codex?.integration_enabled) {
    attentionItems.push(s.server.verdictCodexOff)
  }

  let verdict: string = s.server.verdictStopped
  let verdictTone: 'bad' | 'good' | 'neutral' = 'bad'
  if (status?.running) {
    if (codex?.integration_enabled && (stats?.requests ?? 0) > 0) {
      verdict = s.server.verdictServing.replace('{{count}}', fmt(stats?.requests ?? 0))
      verdictTone = 'good'
    } else if (!codex?.integration_enabled) {
      verdict = s.server.verdictCodexOff
      verdictTone = 'bad'
    } else {
      verdict = s.server.verdictNoTraffic
      verdictTone = 'neutral'
    }
  }

  return (
    <PageShell title={s.server.title} subtitle={s.server.subtitle}>
      <div className="grid grid-cols-[repeat(auto-fit,minmax(280px,1fr))] items-stretch gap-4">
        <Card className="h-full">
          <CardHeader className="flex-row items-center justify-between space-y-0">
            <CardTitle className="text-base">LoomRouter proxy</CardTitle>
            <Badge variant={status?.running ? 'default' : 'secondary'}>
              {status?.running ? s.server.running : s.server.stopped}
            </Badge>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="flex items-start gap-2 rounded-md border border-border bg-muted/40 p-3">
              <div
                className={
                  'mt-0.5 flex h-5 w-5 shrink-0 items-center justify-center rounded-full ' +
                  (verdictTone === 'good'
                    ? 'bg-emerald-600 text-white'
                    : verdictTone === 'bad'
                      ? 'bg-destructive text-white'
                      : 'bg-muted-foreground text-white')
                }
              >
                {verdictTone === 'good' ? (
                  <ShieldCheck className="h-3 w-3" />
                ) : (
                  <AlertTriangle className="h-3 w-3" />
                )}
              </div>
              <div className="min-w-0">
                <p className="text-[11px] leading-none text-muted-foreground">{s.server.summary}</p>
                <p className="mt-1 text-sm font-medium">{verdict}</p>
              </div>
            </div>

            <div className="text-sm">
              <span className="text-muted-foreground">{s.server.port}: </span>
              <code className="font-mono">{status?.port ?? '-'}</code>
            </div>
            {status?.url && (
              <div className="text-sm">
                <span className="text-muted-foreground">{s.server.listeningOn}: </span>
                <code className="font-mono break-all">{status.url}</code>
              </div>
            )}
            <div className="space-y-1.5 border-t border-border pt-3">
              <div className="flex items-center justify-between gap-2 text-sm">
                <span className="text-muted-foreground">{s.server.codexIntegration}</span>
                <span className="flex items-center gap-1.5 font-medium">
                  {codex?.integration_enabled ? (
                    <CheckCircle2 className="h-4 w-4 text-emerald-700 dark:text-emerald-400" />
                  ) : (
                    <XCircle className="h-4 w-4 text-muted-foreground" />
                  )}
                  {codex?.integration_enabled ? s.server.active : s.server.inactive}
                </span>
              </div>
              <div className="flex items-center justify-between gap-2 text-sm">
                <span className="text-muted-foreground">{s.server.enabledProviders}</span>
                <span className="font-mono font-medium">{enabledProviders}</span>
              </div>
              <div className="flex items-center justify-between gap-2 text-sm">
                <span className="text-muted-foreground">{s.server.enabledModels}</span>
                <span className="font-mono font-medium">{enabledModels}</span>
              </div>
              <div className="flex items-center justify-between gap-2 text-sm">
                <span className="text-muted-foreground">{s.server.activeModel}</span>
                <span className="max-w-[60%] truncate font-mono font-medium" title={config?.active_model ?? ''}>
                  {config?.active_model ?? s.codex.activeModelOff}
                </span>
              </div>
            </div>
            <div className="space-y-2 border-t border-border pt-3">
              <label className="text-sm font-medium" htmlFor="sleep-prevention">
                {s.server.sleepPrevention}
              </label>
              <Select
                value={sleepPrevention}
                onValueChange={(value) => void setSleepPrevention(value as SleepPreventionMode)}
              >
                <SelectTrigger id="sleep-prevention" aria-label={s.server.sleepPrevention}>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="while_active">{s.server.sleepDuringActivity}</SelectItem>
                  <SelectItem value="always">{s.server.sleepWhileOn}</SelectItem>
                  <SelectItem value="never">{s.server.sleepNever}</SelectItem>
                </SelectContent>
              </Select>
              <p className="text-xs text-muted-foreground">{s.server.sleepPreventionHint}</p>
            </div>
            {status?.running ? (
              <Button variant="outline" onClick={stop}>
                <Square className="h-4 w-4 mr-2" />
                {s.server.stop}
              </Button>
            ) : (
              <Button onClick={start}>
                <Play className="h-4 w-4 mr-2" />
                {s.server.start}
              </Button>
            )}
          </CardContent>
        </Card>

        <Card className="h-full">
          <CardHeader className="flex-row items-center justify-between space-y-0">
            <CardTitle className="flex items-center gap-2 text-base">
              <Activity className="h-4 w-4 text-muted-foreground" />
              {s.server.trafficTitle}
            </CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <dl className="grid grid-cols-2 gap-x-4 gap-y-4 min-[420px]:grid-cols-3">
              {trafficTiles.map((tile) => (
                <Stat key={tile.label} label={tile.label} value={tile.value} tone={tile.tone} />
              ))}
            </dl>
            {(stats?.requests ?? 0) === 0 && (
              <p className="text-sm text-muted-foreground">{s.server.noTraffic}</p>
            )}
          </CardContent>
        </Card>
      </div>


      {hasProviderTraffic && (
        <Card className="mt-6">
          <CardHeader className="flex-row items-center justify-between space-y-0">
            <CardTitle className="flex items-center gap-2 text-base">
              <Activity className="h-4 w-4 text-muted-foreground" />
              {s.server.trafficByProvider}
            </CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-3">
              <div className="grid grid-cols-[minmax(0,1fr)_auto_auto_auto_auto_auto] items-center gap-x-4 gap-y-1">
                <span className="text-[11px] leading-none text-muted-foreground">{s.server.provider}</span>
                <span className="text-[11px] leading-none text-muted-foreground">{s.overview.requests}</span>
                <span className="text-[11px] leading-none text-muted-foreground">{s.server.share}</span>
                <span className="text-[11px] leading-none text-muted-foreground">{s.server.errors}</span>
                <span className="text-[11px] leading-none text-muted-foreground">{s.server.latency}</span>
                <span className="text-[11px] leading-none text-muted-foreground">{s.server.cost}</span>
              </div>
              {providerRows.map((row) => (
                <div key={row.id} className="grid grid-cols-[minmax(0,1fr)_auto_auto_auto_auto_auto] items-center gap-x-4 gap-y-1 border-t border-border pt-2">
                  <span className="truncate font-mono text-sm font-medium" title={row.name}>{row.name}</span>
                  <span className="font-mono text-sm tabular-nums">{row.requests}</span>
                  <span className="font-mono text-sm tabular-nums text-muted-foreground">{row.share}</span>
                  <span className={'font-mono text-sm tabular-nums ' + (row.errors > 0 ? 'text-destructive' : 'text-muted-foreground')}>{row.errors}</span>
                  <span className="font-mono text-sm tabular-nums text-muted-foreground">{row.latency}</span>
                  <span className="font-mono text-sm tabular-nums">{row.cost}</span>
                </div>
              ))}
            </div>
          </CardContent>
        </Card>
      )}

      {attentionItems.length > 0 && (
        <Card className="mt-6 border-amber-300 bg-amber-50/60 dark:border-amber-900 dark:bg-amber-950/30">
          <CardHeader className="flex-row items-center gap-2 space-y-0">
            <AlertTriangle className="h-4 w-4 text-amber-700 dark:text-amber-400" />
            <CardTitle className="text-base">{s.server.attentionTitle}</CardTitle>
          </CardHeader>
          <CardContent>
            <ul className="space-y-1">
              {attentionItems.map((item) => (
                <li key={item} className="text-sm text-amber-900 dark:text-amber-100">{item}</li>
              ))}
            </ul>
          </CardContent>
        </Card>
      )}

      <Card className="mt-6">
        <CardHeader className="flex-row items-center justify-between space-y-0">
          <CardTitle className="flex items-center gap-2 text-base">
            <Boxes className="h-4 w-4 text-muted-foreground" />
            {s.server.configTitle}
          </CardTitle>
        </CardHeader>
        <CardContent className="grid grid-cols-2 gap-x-4 gap-y-4 sm:grid-cols-4">
          {Object.values(config?.providers ?? {})
            .filter((provider) => provider.enabled)
            .map((provider) => (
              <div key={provider.id} className="min-w-0">
                <p className="truncate text-sm font-medium" title={provider.name}>
                  {provider.name}
                </p>
                <p className="mt-1 text-xs text-muted-foreground">
                  {provider.models.filter((model) => model.enabled).length} / {provider.models.length}
                </p>
              </div>
            ))}
          {enabledProviders === 0 && (
            <p className="text-sm text-muted-foreground col-span-full">{s.overview.noProviders}</p>
          )}
        </CardContent>
      </Card>
    </PageShell>
  )
}
