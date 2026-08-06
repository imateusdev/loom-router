import { useCallback, useEffect, useMemo, useState } from 'react'
import { ArrowDownCircle, ArrowUpCircle, CheckCircle2, Database, RefreshCw, XCircle } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import type { AppConfig, RequestEntry } from '@/types'
import { Card, CardContent } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import PageShell from '@/components/PageShell'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'

const REFRESH_MS = 5_000
const PAGE_SIZE = 200

function fmt(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`
  if (n >= 10_000) return `${(n / 1_000).toFixed(1)}K`
  if (n >= 1_000) return `${(n / 1_000).toFixed(2)}K`
  return String(n)
}

function fmtTime(ts: number): string {
  return new Date(ts * 1000).toLocaleTimeString(undefined, { hour12: false })
}

function fmtLatency(ms: number | null): string {
  if (ms == null) return '—'
  if (ms >= 1000) return `${(ms / 1000).toFixed(1)}s`
  return `${ms}ms`
}

function fmtCost(usd: number | null): string {
  if (usd == null) return '—'
  if (usd < 0.01) return `$${usd.toFixed(4)}`
  return `$${usd.toFixed(2)}`
}

/// Upstream errors are long — a rejected request quotes the provider's whole
/// JSON body. Wrapping it (`break-all`) grew the row to several lines and
/// pushed the rest of the table around, so it is clamped to one line here:
/// hover for the full text, click to expand it in place.
function ErrorText({ text }: { text: string }) {
  const s = useStrings()
  const [open, setOpen] = useState(false)
  return (
    <button
      type="button"
      onClick={() => setOpen((v) => !v)}
      title={open ? s.logs.errorCollapse : text}
      aria-expanded={open}
      className={
        // Narrower where the pane is tight: a fixed 16rem made the status
        // column the widest in the table at the 900px window minimum.
        'mt-1 block max-w-48 cursor-pointer text-left text-xs text-destructive underline-offset-2 hover:underline lg:max-w-64 ' +
        // `whitespace-normal` is required: TableCell sets nowrap, which wins
        // over `break-all` and keeps the expanded text on one overflowing
        // line instead of wrapping inside the cell.
        (open ? 'whitespace-normal break-all' : 'truncate')
      }
    >
      {text}
    </button>
  )
}

export default function LogsPage() {
  const s = useStrings()
  const [entries, setEntries] = useState<RequestEntry[]>([])
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [updatedAt, setUpdatedAt] = useState<Date | null>(null)
  const [providerFilter, setProviderFilter] = useState('all')
  const [statusFilter, setStatusFilter] = useState('all')
  const [refreshing, setRefreshing] = useState(false)

  useEffect(() => {
    api.getConfig().then(setConfig)
  }, [])

  // One loader for both the 5s poll and the manual button, so the two paths
  // cannot drift. Only a manual refresh spins the icon: doing it on every
  // tick would leave the header permanently twitching.
  const load = useCallback(async (manual = false) => {
    if (manual) setRefreshing(true)
    try {
      const rows = await api.recentRequests(PAGE_SIZE)
      setEntries(rows)
      setUpdatedAt(new Date())
    } catch {
      // Keep the rows already on screen; the next tick tries again.
    } finally {
      if (manual) setRefreshing(false)
    }
  }, [])

  useEffect(() => {
    let timer: ReturnType<typeof setInterval> | undefined
    const start = () => {
      load()
      // Wrapped: setInterval would otherwise hand the callback its own
      // arguments, and `manual` must stay false here.
      timer = setInterval(() => load(), REFRESH_MS)
    }
    const stop = () => {
      if (timer !== undefined) {
        clearInterval(timer)
        timer = undefined
      }
    }
    // Pause polling while the window/tab is hidden; resume on return.
    const onVisibility = () => {
      if (document.hidden) stop()
      else start()
    }
    if (!document.hidden) start()
    document.addEventListener('visibilitychange', onVisibility)
    return () => {
      stop()
      document.removeEventListener('visibilitychange', onVisibility)
    }
  }, [load])

  const providerName = useMemo(() => {
    const map = new Map<string, string>()
    for (const p of Object.values(config?.providers ?? {})) map.set(p.id, p.name)
    map.set('codex-native', s.overview.native)
    return (id: string) => map.get(id) ?? id
  }, [config, s])

  const providerIds = useMemo(
    () => [...new Set(entries.map((e) => e.provider))].sort(),
    [entries],
  )

  const filtered = useMemo(
    () =>
      entries.filter(
        (e) =>
          (providerFilter === 'all' || e.provider === providerFilter) &&
          (statusFilter === 'all' || e.status === statusFilter),
      ),
    [entries, providerFilter, statusFilter],
  )

  return (
    <PageShell
      title={s.logs.title}
      subtitle={
        <>
          {s.logs.subtitle}
          {updatedAt && ` · ${s.logs.updatedAt} ${fmtTime(updatedAt.getTime() / 1000)}`}
        </>
      }
      actions={
        <>
          <Select value={providerFilter} onValueChange={setProviderFilter}>
            <SelectTrigger
              className="w-auto min-w-44 max-w-[18rem]"
              title={providerFilter === 'all' ? s.logs.allProviders : providerName(providerFilter)}
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">{s.logs.allProviders}</SelectItem>
              {providerIds.map((id) => (
                <SelectItem key={id} value={id}>
                  {providerName(id)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Select value={statusFilter} onValueChange={setStatusFilter}>
            <SelectTrigger
              className="w-auto min-w-36 max-w-[14rem]"
              title={statusFilter === 'all' ? s.logs.allStatuses : statusFilter === 'error' ? s.logs.failed : 'ok'}
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="all">{s.logs.allStatuses}</SelectItem>
              <SelectItem value="ok">ok</SelectItem>
              <SelectItem value="error">{s.logs.failed}</SelectItem>
            </SelectContent>
          </Select>
          {/* The auto-refresh chip used to be a static badge. It states a
              behaviour the user cannot influence, which is exactly where a
              button belongs: same label, now also refreshes on demand. */}
          <Button
            variant="secondary"
            size="sm"
            onClick={() => load(true)}
            disabled={refreshing}
            title={s.logs.refreshNow}
            aria-label={s.logs.refreshNow}
            className="gap-1.5"
          >
            <RefreshCw className={'h-3 w-3' + (refreshing ? ' animate-spin' : '')} />
            {s.logs.autoRefresh}
          </Button>
        </>
      }
    >
      <Card>
        {/* The table is wider than the card on a narrow window; scroll it
            inside its own container instead of stretching the page. */}
        <CardContent className="overflow-x-auto p-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">{s.logs.time}</TableHead>
                <TableHead>{s.logs.provider}</TableHead>
                <TableHead>{s.logs.model}</TableHead>
                <TableHead>{s.logs.status}</TableHead>
                <TableHead className="text-right">{s.logs.latency}</TableHead>
                <TableHead className="text-right pr-6 xl:pr-4">{s.logs.cost}</TableHead>
                {/* Token counts drop out below xl. The table wants ~745px and
                    the pane is the window minus a 240px sidebar, so at the
                    900px window minimum it only has ~610 and would scroll
                    sideways. These three are the secondary read — the same
                    totals are on Overview — while time, provider, model,
                    status and latency are why the page exists. */}
                <TableHead className="hidden text-right xl:table-cell">
                  <span className="inline-flex items-center gap-1">
                    <ArrowUpCircle className="h-3.5 w-3.5" />
                    {s.logs.input}
                  </span>
                </TableHead>
                <TableHead className="hidden text-right xl:table-cell">
                  <span className="inline-flex items-center gap-1">
                    <ArrowDownCircle className="h-3.5 w-3.5" />
                    {s.logs.output}
                  </span>
                </TableHead>
                <TableHead className="hidden text-right xl:table-cell xl:pr-6">
                  <span className="inline-flex items-center gap-1">
                    <Database className="h-3.5 w-3.5" />
                    {s.logs.cached}
                  </span>
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {filtered.map((e, i) => (
                <TableRow key={`${e.ts}-${i}`}>
                  <TableCell className="pl-6 font-mono text-xs text-muted-foreground">
                    {fmtTime(e.ts)}
                  </TableCell>
                  {/* TableCell is nowrap by default, so a name like "Codex
                      native (ChatGPT)" held the column at its full length and
                      pushed the table past the pane. Let it wrap while the
                      window is narrow; there is room for one line above lg. */}
                  <TableCell className="whitespace-normal xl:whitespace-nowrap">
                    <div className="text-sm">{providerName(e.provider)}</div>
                    <div className="text-xs text-muted-foreground uppercase">{e.transport}</div>
                  </TableCell>
                  <TableCell className="max-w-[22ch] truncate font-mono text-xs" title={e.model}>
                    {e.model}
                  </TableCell>
                  <TableCell>
                    {e.status === 'ok' ? (
                      <Badge variant="default" className="gap-1">
                        <CheckCircle2 className="h-3 w-3" />
                        ok
                      </Badge>
                    ) : (
                      <div>
                        <Badge variant="destructive" className="gap-1">
                          <XCircle className="h-3 w-3" />
                          {s.logs.failed}
                        </Badge>
                        {e.error && <ErrorText text={e.error} />}
                      </div>
                    )}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs">
                    {fmtLatency(e.latency_ms)}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs pr-6 xl:pr-4">
                    {e.status === 'ok' ? fmtCost(e.cost_usd) : '—'}
                  </TableCell>
                  {/* Hidden with their headers below lg; see the header note. */}
                  <TableCell className="hidden text-right font-mono text-xs xl:table-cell">
                    {e.status === 'ok' ? fmt(e.input_tokens) : '—'}
                  </TableCell>
                  <TableCell className="hidden text-right font-mono text-xs xl:table-cell">
                    {e.status === 'ok' ? fmt(e.output_tokens) : '—'}
                  </TableCell>
                  <TableCell className="hidden text-right font-mono text-xs xl:table-cell xl:pr-6">
                    {e.status === 'ok' ? fmt(e.cached_tokens) : '—'}
                  </TableCell>
                </TableRow>
              ))}
              {filtered.length === 0 && (
                <TableRow>
                  <TableCell colSpan={9} className="pl-6 py-10 text-center">
                    <p className="text-sm text-muted-foreground">{s.logs.empty}</p>
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </CardContent>
      </Card>
    </PageShell>
  )
}
