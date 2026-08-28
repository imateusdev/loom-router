import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router'
import { AlertTriangle, CheckCircle2, X, XCircle } from 'lucide-react'
import { api } from '@/lib/api'
import { useBackendState } from '@/lib/events'
import { useLocale, useStrings, type Locale } from '@/i18n'
import { avgCostPerRequest, formatContextWindow } from '@/lib/utils'
import type {
  AppConfig,
  ContextWindow,
  KeyUsage,
  ModelAggregate,
  ProviderBalance,
  SetupStatus,
  StatsSummary,
} from '@/types'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { AnalyticsChart, type ChartPoint } from '@/components/AnalyticsChart'
import PageShell from '@/components/PageShell'
import { Badge } from '@/components/ui/badge'
import { Progress } from '@/components/ui/progress'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs'

type PeriodKey = 'today' | 'h24' | 'd7' | 'd30'

const SETUP_BANNER_DISMISSED_KEY = 'loomrouter.setup-banner.dismissed'

const QUOTA_LOCALE: Record<Locale, string> = {
  en: 'en',
  pt: 'pt-BR',
  es: 'es',
  zh: 'zh-CN',
}

function formatResetAt(resetAt: string | null | undefined, locale: Locale): string | null {
  if (!resetAt) return null
  const date = new Date(resetAt)
  if (Number.isNaN(date.getTime())) return null
  return new Intl.DateTimeFormat(QUOTA_LOCALE[locale], {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  }).format(date)
}

function periodSecs(key: PeriodKey): number {
  switch (key) {
    case 'h24':
      return 86_400
    case 'd7':
      return 7 * 86_400
    case 'd30':
      return 30 * 86_400
    case 'today': {
      const now = new Date()
      const midnight = new Date(now.getFullYear(), now.getMonth(), now.getDate())
      return Math.max(60, Math.floor((now.getTime() - midnight.getTime()) / 1000))
    }
  }
}

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

/// One labelled stat. Label above value, so the numbers stay scannable in a
/// column instead of running together in a sentence.
function Stat({ label, value, tone }: { label: string; value: string; tone?: 'bad' | 'good' }) {
  return (
    <div className="min-w-0 shrink-0">
      <dt className="text-[11px] leading-none text-muted-foreground">{label}</dt>
      <dd
        className={
          'mt-1 font-mono text-sm tabular-nums ' + (tone === 'bad' ? 'text-destructive' : tone === 'good' ? 'text-emerald-700 dark:text-emerald-400' : '')
        }
      >
        {value}
      </dd>
    </div>
  )
}

/// A model and how it actually behaved: speed, cache efficiency, volume,
/// cost and failures - the characteristics you compare models on.
function ModelRow({ m, window: ctx }: { m: ModelAggregate; window?: ContextWindow }) {
  const s = useStrings()
  return (
    <div className="rounded-lg border border-border p-3">
      <div className="mb-2 flex items-center justify-between gap-2">
        <div className="flex min-w-0 items-center gap-2">
          <span className="min-w-0 truncate font-mono text-sm" title={m.model}>
            {m.model}
          </span>
          {ctx && (
            <span
              title={ctx.known ? s.providers.contextKnown : s.providers.contextGuess}
              className={
                'shrink-0 rounded-full border px-2 py-0.5 font-mono text-[11px] leading-none ' +
                (ctx.known
                  ? 'border-border text-muted-foreground'
                  : 'border-dashed border-border text-muted-foreground/60')
              }
            >
              {formatContextWindow(ctx.window)}
              {!ctx.known && ' ?'}
            </span>
          )}
        </div>
        {m.errors > 0 && (
          <Badge variant="destructive" className="shrink-0">
            {m.errors} {s.overview.failures}
          </Badge>
        )}
      </div>
      {/* Reflows to whatever the pane allows; never a fixed column count. */}
      <dl className="flex flex-wrap gap-x-4 gap-y-3">
        <Stat label={s.overview.reqShort} value={String(m.requests)} />
        <Stat label={s.overview.avgLatency} value={fmtLatency(m.avg_latency_ms)} />
        <Stat label={s.overview.cacheRatio} value={`${Math.round(m.cache_ratio * 100)}%`} />
        <Stat label={s.overview.inputTokens} value={fmt(m.input_tokens)} />
        <Stat label={s.overview.outputTokens} value={fmt(m.output_tokens)} />
        <Stat
          label={s.overview.estCost}
          value={m.cost_usd != null ? `$${m.cost_usd.toFixed(2)}` : '-'}
          tone="good"
        />
      </dl>
    </div>
  )
}

function KeyRow({
  balance,
  usage,
  accountLevel,
}: {
  balance: ProviderBalance
  usage?: KeyUsage
  accountLevel: boolean
}) {
  const s = useStrings()
  const locale = useLocale()
  const tokenReported =
    usage != null && (usage.requests === 0 || usage.input_tokens > 0 || usage.output_tokens > 0)
  return (
    <div className="rounded-lg border border-border p-3">
      <div className="mb-2 flex flex-wrap items-center gap-2">
        <span className="min-w-0 flex-1 truncate text-sm font-medium">
          {balance.key_name ?? s.overview.accountLevel}
        </span>
        {accountLevel && (
          <Badge variant="outline" className="shrink-0">
            {s.overview.accountLevel}
          </Badge>
        )}
        <Badge variant={balance.ok ? 'default' : 'secondary'} className="gap-1">
          {balance.ok ? <CheckCircle2 className="h-3 w-3" /> : <XCircle className="h-3 w-3" />}
          {balance.ok ? 'ok' : s.overview.unreachable}
        </Badge>
      </div>
      <dl className="flex flex-wrap gap-x-4 gap-y-3">
        <Stat label={s.overview.reqShort} value={String(usage?.requests ?? 0)} />
        <Stat
          label={s.overview.inputTokens}
          value={usage != null && tokenReported ? fmt(usage.input_tokens) : s.overview.notReported}
        />
        <Stat
          label={s.overview.outputTokens}
          value={usage != null && tokenReported ? fmt(usage.output_tokens) : s.overview.notReported}
        />
        <Stat
          label={s.overview.cacheTokens}
          value={usage != null && tokenReported ? fmt(usage.cached_tokens) : s.overview.notReported}
        />
      </dl>
      {balance.bars.map((bar) => {
        const resetAt = formatResetAt(bar.reset_at, locale)
        return (
          <div key={bar.label} className="mt-3">
            <div className="mb-1 flex justify-between text-xs">
              <span className="text-muted-foreground">{bar.label}</span>
              <span className="font-medium">{Math.round(bar.percent)}%</span>
            </div>
            <Progress value={bar.percent} className="h-2" />
            <p className="mt-1 text-xs text-muted-foreground">
              {resetAt ? `${bar.detail} · ${s.overview.resetsAt} ${resetAt}` : bar.detail}
            </p>
          </div>
        )
      })}
      {balance.balance_text ? (
        <p className="mt-3 text-lg font-semibold text-emerald-700 dark:text-emerald-400">
          {balance.balance_text}
        </p>
      ) : (
        <p className="mt-3 text-xs text-muted-foreground">{s.overview.notReported}</p>
      )}
      {balance.error && <p className="mt-1 text-xs text-destructive break-all">{balance.error}</p>}
    </div>
  )
}

function OverviewSkeleton() {
  return (
    <>
      <div className="mb-6 grid grid-cols-[repeat(auto-fit,minmax(280px,1fr))] items-stretch gap-4">
        {[0, 1].map((i) => (
          <Card key={i} className="min-w-0">
            <CardHeader className="flex-row items-center justify-between space-y-0 pb-2">
              <div className="h-4 w-36 rounded bg-muted animate-pulse" />
              <div className="h-5 w-14 rounded-full bg-muted animate-pulse" />
            </CardHeader>
            <CardContent className="space-y-3">
              <div className="h-2 w-full rounded-full bg-muted animate-pulse" />
              <div className="h-2 w-2/3 rounded-full bg-muted animate-pulse" />
              <div className="h-6 w-20 rounded bg-muted animate-pulse" />
            </CardContent>
          </Card>
        ))}
      </div>
      <div className="mb-6 grid grid-cols-3 gap-4 min-[1392px]:grid-cols-6">
        {Array.from({ length: 6 }, (_, i) => (
          <Card key={i} className="min-w-0">
            <CardContent className="pt-6">
              <div className="h-[17px] w-24 rounded bg-muted animate-pulse" />
              <div className="mt-1 h-[33px] w-20 rounded bg-muted animate-pulse" />
            </CardContent>
          </Card>
        ))}
      </div>
      <Card className="mb-6">
        <CardHeader>
          <div className="h-4 w-24 rounded bg-muted animate-pulse" />
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="h-4 w-full rounded bg-muted animate-pulse" />
          <div className="h-4 w-2/3 rounded bg-muted animate-pulse" />
          <div className="h-4 w-1/2 rounded bg-muted animate-pulse" />
        </CardContent>
      </Card>
    </>
  )
}

export default function OverviewPage() {
  const s = useStrings()
  const locale = useLocale()
  const [period, setPeriod] = useState<PeriodKey>('h24')
  const [stats, setStats] = useState<StatsSummary | null>(null)
  const [loading, setLoading] = useState(true)
  const [balances, setBalances] = useState<ProviderBalance[]>([])
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [setupStatus, setSetupStatus] = useState<SetupStatus | null>(null)
  const [bannerDismissed, setBannerDismissed] = useState(() => {
    try {
      return window.sessionStorage.getItem(SETUP_BANNER_DISMISSED_KEY) === '1'
    } catch {
      return false
    }
  })

  // Context windows are a model characteristic too; read from the backend so
  // the figure matches the one published to Codex.
  const [windows, setWindows] = useState<Record<string, ContextWindow> | null>(null)

  // One loader for everything visible on this page. The skeleton stays up
  // until both the stats and the provider/balance reads have landed, so the
  // "no providers yet" empty state can never flash before real data.
  const loadAll = useCallback(() => {
    return Promise.all([
      api.getConfig(),
      api.providerBalances(),
      api.contextWindows(),
      api.setupStatus(),
      api.statsSummary(periodSecs(period)),
    ])
      .then(([cfg, bal, win, setup, st]) => {
        setConfig(cfg)
        setBalances(bal)
        setWindows(win)
        setSetupStatus(setup)
        setStats(st)
      })
      .catch(() => {})
      .finally(() => setLoading(false))
  }, [period])

  useEffect(() => {
    void loadAll()
  }, [loadAll])
  // Providers can be switched off from the tray while this page is open.
  useBackendState(loadAll)

  const providerName = useMemo(() => {
    const map = new Map<string, string>()
    for (const p of Object.values(config?.providers ?? {})) map.set(p.id, p.name)
    map.set('codex-native', s.overview.native)
    return (id: string) => map.get(id) ?? id
  }, [config, s])

  const chartData = useMemo(() => {
    // The stored model is a `namespace/slug`, and the namespace is not always
    // the routing provider, so key by the bare slug to collapse the same
    // upstream model served through several gateways into one bubble.
    const byModel = new Map<
      string,
      { requests: number; costUsd: number; latencyWeighted: number; providers: Set<string> }
    >()
    for (const provider of stats?.per_provider ?? []) {
      for (const model of provider.models) {
        if (
          model.cost_usd == null ||
          model.cost_usd <= 0 ||
          model.avg_latency_ms == null ||
          model.requests === 0
        ) {
          continue
        }
        const label = model.model.slice(model.model.lastIndexOf('/') + 1)
        const agg = byModel.get(label) ?? {
          requests: 0,
          costUsd: 0,
          latencyWeighted: 0,
          providers: new Set<string>(),
        }
        agg.requests += model.requests
        agg.costUsd += model.cost_usd
        agg.latencyWeighted += model.avg_latency_ms * model.requests
        agg.providers.add(providerName(provider.provider))
        byModel.set(label, agg)
      }
    }
    const points: ChartPoint[] = []
    for (const [label, agg] of byModel) {
      points.push({
        key: label,
        label,
        sublabel: [...agg.providers].join(', '),
        cost: avgCostPerRequest(agg.costUsd, agg.requests),
        latencyMs: Math.round(agg.latencyWeighted / agg.requests),
        requests: agg.requests,
      })
    }
    return points
  }, [stats, providerName])

  const balanceGroups = useMemo(() => {
    const groups = new Map<string, ProviderBalance[]>()
    for (const balance of balances) {
      const list = groups.get(balance.provider_id) ?? []
      list.push(balance)
      groups.set(balance.provider_id, list)
    }
    return [...groups.values()]
  }, [balances])

  const tiles = [
    { label: s.overview.requests, value: fmt(stats?.requests ?? 0) },
    { label: s.overview.inputTokens, value: fmt(stats?.input_tokens ?? 0) },
    { label: s.overview.outputTokens, value: fmt(stats?.output_tokens ?? 0) },
    { label: s.overview.cacheTokens, value: fmt(stats?.cached_tokens ?? 0) },
    {
      label: s.overview.cacheRatio,
      value: `${Math.round((stats?.cache_ratio ?? 0) * 100)}%`,
    },
    { label: s.overview.estCost, value: `$${(stats?.cost_usd ?? 0).toFixed(2)}` },
  ]

  const dismissBanner = () => {
    setBannerDismissed(true)
    try {
      window.sessionStorage.setItem(SETUP_BANNER_DISMISSED_KEY, '1')
    } catch {
      // Session-only dismissal is best-effort; the banner can reappear.
    }
  }

  const missingItems = (setupStatus?.missing ?? []).map((item) => {
    if (item === 'codex_integration') {
      return { label: s.overview.setupMissingCodex, to: '/codex' }
    }
    if (item === 'provider') {
      return { label: s.overview.setupMissingProvider, to: '/providers' }
    }
    return { label: s.overview.setupMissingModel, to: '/providers' }
  })

  return (
    <PageShell
      title={s.overview.title}
      subtitle={s.overview.subtitle}
      actions={
        <Tabs value={period} onValueChange={(v) => setPeriod(v as PeriodKey)}>
          <TabsList>
            <TabsTrigger value="today">{s.overview.today}</TabsTrigger>
            <TabsTrigger value="h24">{s.overview.h24}</TabsTrigger>
            <TabsTrigger value="d7">{s.overview.d7}</TabsTrigger>
            <TabsTrigger value="d30">{s.overview.d30}</TabsTrigger>
          </TabsList>
        </Tabs>
      }
    >
      {setupStatus && !setupStatus.ready && !bannerDismissed && (
        <div className="mb-6 flex gap-3 rounded-lg border border-amber-500/30 bg-amber-500/10 p-4 text-amber-700 dark:text-amber-300">
          <AlertTriangle className="mt-0.5 h-5 w-5 shrink-0" />
          <div className="min-w-0 flex-1">
            <p className="font-medium">{s.overview.setupPendingTitle}</p>
            <p className="mt-1 text-sm text-amber-700/80 dark:text-amber-300/80">
              {s.overview.setupPendingBody}
            </p>
            <ul className="mt-3 flex flex-wrap gap-2">
              {missingItems.map((item) => (
                <li key={item.to + item.label}>
                  <Link
                    to={item.to}
                    className="inline-flex items-center rounded-md border border-current px-2.5 py-1 text-xs font-medium underline-offset-4 hover:underline"
                  >
                    {item.label}
                  </Link>
                </li>
              ))}
            </ul>
          </div>
          <Button
            variant="ghost"
            size="icon"
            onClick={dismissBanner}
            aria-label={s.overview.dismissSetupBanner}
          >
            <X className="h-4 w-4" />
          </Button>
        </div>
      )}

      <Tabs defaultValue="general">
        <TabsList>
          <TabsTrigger value="general">{s.overview.tabGeneral}</TabsTrigger>
          <TabsTrigger value="analytics">{s.overview.tabAnalytics}</TabsTrigger>
        </TabsList>
        <TabsContent value="general">
        {loading ? (
        <OverviewSkeleton />
      ) : (
        <>
      {/* Provider cards: quota bars and balances.
          `auto-fit` rather than md/xl breakpoints - those measure the whole
          window, but this grid only ever gets the window minus the sidebar,
          so they fired roughly 240px too late. */}
      <div className="mb-6 grid grid-cols-[repeat(auto-fit,minmax(280px,1fr))] items-stretch gap-4">
        {balanceGroups.map((group) => {
          // Every row has to have reported the same text: two keys that merely
          // both read "$0.00" are still two accounts, and a set built from the
          // reporting rows alone labelled them as one.
          const reported = group.map((balance) => balance.balance_text).filter(Boolean)
          const accountLevel =
            group.length > 1 && reported.length === group.length && new Set(reported).size === 1
          return (
            <Card key={group[0].provider_id} className="h-full">
              <CardHeader className="flex-row items-center justify-between space-y-0 pb-2">
                <div className="min-w-0">
                  <CardTitle className="text-base">
                    {providerName(group[0].provider_id)}
                  </CardTitle>
                  <p className="mt-0.5 text-[10px] font-mono uppercase tracking-wide text-muted-foreground">
                    {group[0].provider_id === 'claude-code'
                      ? s.providers.modePlan
                      : s.providers.modeApi}
                  </p>
                </div>
              </CardHeader>
              <CardContent className="space-y-3">
                {group.map((balance) => (
                  <KeyRow
                    key={balance.key_id ?? `${balance.provider_id}-account`}
                    balance={balance}
                    // ponytail: `?.` on per_key too - a throw here unmounts the
                    // whole tree into a blank window, so never trust the field
                    // to be present just because the type says it is.
                    usage={stats?.per_key?.find((usage) => usage.key_id === balance.key_id)}
                    accountLevel={accountLevel}
                  />
                ))}
              </CardContent>
            </Card>
          )
        })}
        {balances.length === 0 && (
          <Card className="min-w-0 min-h-[180px]">
            <CardContent className="flex flex-1 items-center justify-center">
              <p className="text-sm text-muted-foreground">{s.overview.noProviders}</p>
            </CardContent>
          </Card>
        )}
      </div>

      {/* Exactly six tiles, so the column counts are divisors of six.
          `auto-fit` is wrong for a fixed-size set: it fitted five across and
          left the sixth stranded alone on the next row. */}
      <div className="mb-6 grid grid-cols-3 gap-4 min-[1392px]:grid-cols-6">
        {tiles.map((t) => (
          <Card key={t.label} className="h-full">
            <CardContent className="pt-6">
              <p className="text-[17px] text-muted-foreground">{t.label}</p>
              <p className={"text-[33px] font-semibold mt-1 " + (t.value.startsWith('$') ? 'text-emerald-700 dark:text-emerald-400' : '')}>{t.value}</p>
            </CardContent>
          </Card>
        ))}
      </div>
      <p className="text-xs text-muted-foreground -mt-4 mb-6">{s.logs.estCostDisclaimer}</p>

      {/* Usage broken down to the model.
          This was one right-aligned run-on string per provider
          ("2 req · 180 in · 152 out · 0 cached"), which hid the thing worth
          comparing: the models. The backend always grouped by (provider,
          model) and folded the model away - it is kept now, so each model
          gets its own labelled row. */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{s.overview.requests}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-6">
          {(stats?.per_provider ?? []).map((p) => (
            <section key={p.provider}>
              <div className="mb-2 flex flex-wrap items-baseline gap-x-3 gap-y-1 border-b border-border pb-2">
                <h3 className="text-sm font-medium">{providerName(p.provider)}</h3>
                <span className="text-xs text-muted-foreground">
                  {p.requests} {s.overview.reqShort}
                  {p.cost_usd != null && (
                    <span className="text-emerald-700 dark:text-emerald-400">
                      {' '}· ${p.cost_usd.toFixed(2)}
                    </span>
                  )}
                </span>
              </div>
              <div className="space-y-2">
                {p.models.map((m) => (
                  <ModelRow key={m.model} m={m} window={windows?.[m.model]} />
                ))}
              </div>
            </section>
          ))}
          {(stats?.per_provider?.length ?? 0) === 0 && (
            <p className="text-sm text-muted-foreground">{s.overview.noRequests}</p>
          )}
        </CardContent>
      </Card>
        </>
      )}
        </TabsContent>
        <TabsContent value="analytics">
          {loading ? (
            <div
              className="h-[460px] animate-pulse rounded-lg border bg-muted/40"
              aria-hidden="true"
            />
          ) : (
            <AnalyticsChart
              data={chartData}
              locale={locale}
              title={s.overview.chartTitle}
              subtitle={s.overview.chartSubtitle}
              frontierLegend={s.overview.frontierLegend}
              axisCost={s.overview.axisCost}
              axisSpeed={s.overview.axisSpeed}
              empty={s.overview.chartEmpty}
              bubbleLegend={s.overview.bubbleLegend}
            />
          )}
        </TabsContent>
      </Tabs>
    </PageShell>
  )
}
