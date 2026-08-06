import { useEffect, useMemo, useState } from 'react'
import { CheckCircle2, XCircle } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import type {
  AppConfig,
  ContextWindow,
  ModelAggregate,
  ProviderBalance,
  StatsSummary,
} from '@/types'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import PageShell from '@/components/PageShell'
import { Badge } from '@/components/ui/badge'
import { Progress } from '@/components/ui/progress'
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/tabs'

type PeriodKey = 'today' | 'h24' | 'd7' | 'd30'

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
  if (ms == null) return '—'
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${ms}ms`
}

/// One labelled stat. Label above value, so the numbers stay scannable in a
/// column instead of running together in a sentence.
function Stat({ label, value, tone }: { label: string; value: string; tone?: 'bad' }) {
  return (
    <div className="min-w-0">
      <dt className="text-[11px] leading-none text-muted-foreground">{label}</dt>
      <dd
        className={
          'mt-1 font-mono text-sm tabular-nums ' + (tone === 'bad' ? 'text-destructive' : '')
        }
      >
        {value}
      </dd>
    </div>
  )
}

/// A model and how it actually behaved: speed, cache efficiency, volume,
/// cost and failures — the characteristics you compare models on.
function ModelRow({ m, window: ctx }: { m: ModelAggregate; window?: ContextWindow }) {
  const s = useStrings()
  return (
    <div className="rounded-lg border border-border p-3">
      <div className="mb-2 flex flex-wrap items-center gap-2">
        <span className="truncate font-mono text-sm" title={m.model}>
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
            {ctx.window >= 1_000_000
              ? `${ctx.window / 1_000_000}M`
              : `${Math.round(ctx.window / 1024)}K`}
            {!ctx.known && ' ?'}
          </span>
        )}
        {m.errors > 0 && (
          <Badge variant="destructive" className="shrink-0">
            {m.errors} {s.overview.failures}
          </Badge>
        )}
      </div>
      {/* Reflows to whatever the pane allows; never a fixed column count. */}
      <dl className="grid grid-cols-[repeat(auto-fit,minmax(76px,1fr))] gap-x-4 gap-y-3">
        <Stat label={s.overview.reqShort} value={String(m.requests)} />
        <Stat label={s.overview.avgLatency} value={fmtLatency(m.avg_latency_ms)} />
        <Stat label={s.overview.cacheRatio} value={`${Math.round(m.cache_ratio * 100)}%`} />
        <Stat label={s.overview.inputTokens} value={fmt(m.input_tokens)} />
        <Stat label={s.overview.outputTokens} value={fmt(m.output_tokens)} />
        <Stat
          label={s.overview.estCost}
          value={m.cost_usd != null ? `$${m.cost_usd.toFixed(2)}` : '—'}
        />
      </dl>
    </div>
  )
}

export default function OverviewPage() {
  const s = useStrings()
  const [period, setPeriod] = useState<PeriodKey>('h24')
  const [stats, setStats] = useState<StatsSummary | null>(null)
  const [balances, setBalances] = useState<ProviderBalance[]>([])
  const [config, setConfig] = useState<AppConfig | null>(null)

  // Context windows are a model characteristic too; read from the backend so
  // the figure matches the one published to Codex.
  const [windows, setWindows] = useState<Record<string, ContextWindow> | null>(null)

  useEffect(() => {
    // Catches: a failed read here costs a provider label, not the page,
    // and an unhandled rejection is noise that hides real errors.
    api.getConfig().then(setConfig).catch(() => {})
    api.providerBalances().then(setBalances).catch(() => setBalances([]))
    api.contextWindows().then(setWindows).catch(() => setWindows(null))
  }, [])

  useEffect(() => {
    api.statsSummary(periodSecs(period)).then(setStats)
  }, [period])

  const providerName = useMemo(() => {
    const map = new Map<string, string>()
    for (const p of Object.values(config?.providers ?? {})) map.set(p.id, p.name)
    map.set('codex-native', s.overview.native)
    return (id: string) => map.get(id) ?? id
  }, [config, s])

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
      {/* Provider cards: quota bars and balances.
          `auto-fit` rather than md/xl breakpoints — those measure the whole
          window, but this grid only ever gets the window minus the sidebar,
          so they fired roughly 240px too late. */}
      <div className="mb-6 grid grid-cols-[repeat(auto-fit,minmax(280px,1fr))] items-start gap-4">
        {balances.map((b) => (
          <Card key={b.provider_id}>
            <CardHeader className="flex-row items-center justify-between space-y-0 pb-2">
              <div className="min-w-0">
                <CardTitle className="text-base">{providerName(b.provider_id)}</CardTitle>
                {/* Provider ids are single unbroken tokens and the card floor
                    is 280px, so this has to be able to clip. */}
                <p
                  className="mt-0.5 truncate font-mono text-xs text-muted-foreground"
                  title={b.provider_id}
                >
                  {b.provider_id}
                </p>
              </div>
              <Badge variant={b.ok ? 'default' : 'secondary'} className="gap-1">
                {b.ok ? (
                  <CheckCircle2 className="h-3 w-3" />
                ) : (
                  <XCircle className="h-3 w-3" />
                )}
                {b.ok ? 'ok' : s.overview.unreachable}
              </Badge>
            </CardHeader>
            <CardContent className="space-y-3">
              {b.bars.map((bar) => (
                <div key={bar.label}>
                  <div className="flex justify-between text-xs mb-1">
                    <span className="text-muted-foreground">{bar.label}</span>
                    <span className="font-medium">{Math.round(bar.percent)}%</span>
                  </div>
                  <Progress value={bar.percent} className="h-2" />
                  <p className="text-xs text-muted-foreground mt-1">{bar.detail}</p>
                </div>
              ))}
              {b.balance_text && (
                <div>
                  <p className="text-xs text-muted-foreground">{s.overview.balance}</p>
                  <p className="text-xl font-semibold">{b.balance_text}</p>
                </div>
              )}
              {b.error && <p className="text-xs text-destructive break-all">{b.error}</p>}
            </CardContent>
          </Card>
        ))}
        {balances.length === 0 && (
          <p className="text-sm text-muted-foreground">{s.overview.noProviders}</p>
        )}
      </div>

      {/* Exactly six tiles, so the column counts are divisors of six.
          `auto-fit` is wrong for a fixed-size set: it fitted five across and
          left the sixth stranded alone on the next row. */}
      <div className="mb-6 grid grid-cols-3 gap-4 min-[1392px]:grid-cols-6">
        {tiles.map((t) => (
          <Card key={t.label}>
            <CardContent className="pt-6">
              <p className="text-xs text-muted-foreground">{t.label}</p>
              <p className="text-2xl font-semibold mt-1">{t.value}</p>
            </CardContent>
          </Card>
        ))}
      </div>
      <p className="text-xs text-muted-foreground -mt-4 mb-6">{s.logs.estCostDisclaimer}</p>

      {/* Usage broken down to the model.
          This was one right-aligned run-on string per provider
          ("2 req · 180 in · 152 out · 0 cached"), which hid the thing worth
          comparing: the models. The backend always grouped by (provider,
          model) and folded the model away — it is kept now, so each model
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
                  {p.cost_usd != null && ` · $${p.cost_usd.toFixed(2)}`}
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
    </PageShell>
  )
}
