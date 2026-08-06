import { useEffect, useMemo, useState } from 'react'
import { CheckCircle2, XCircle } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import type { AppConfig, ProviderBalance, StatsSummary } from '@/types'
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

export default function OverviewPage() {
  const s = useStrings()
  const [period, setPeriod] = useState<PeriodKey>('h24')
  const [stats, setStats] = useState<StatsSummary | null>(null)
  const [balances, setBalances] = useState<ProviderBalance[]>([])
  const [config, setConfig] = useState<AppConfig | null>(null)

  useEffect(() => {
    api.getConfig().then(setConfig)
    api.providerBalances().then(setBalances)
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
              <div>
                <CardTitle className="text-base">{providerName(b.provider_id)}</CardTitle>
                <p className="text-xs text-muted-foreground font-mono mt-0.5">{b.provider_id}</p>
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
      <div className="mb-6 grid grid-cols-2 gap-4 md:grid-cols-3 2xl:grid-cols-6">
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

      {/* Per-provider usage */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{s.overview.requests}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-2">
          {(stats?.per_provider ?? []).map((p) => (
            <div key={p.provider} className="flex items-center justify-between text-sm">
              <span>{providerName(p.provider)}</span>
              <span className="text-muted-foreground">
                {p.requests} req · {fmt(p.input_tokens)} in · {fmt(p.output_tokens)} out ·{' '}
                {fmt(p.cached_tokens)} cached
                {p.cost_usd != null && ` · $${p.cost_usd.toFixed(2)}`}
              </span>
            </div>
          ))}
        </CardContent>
      </Card>
    </PageShell>
  )
}
