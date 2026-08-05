import { useEffect, useMemo, useState } from 'react'
import { ArrowDownCircle, ArrowUpCircle, CheckCircle2, Database, RefreshCw, XCircle } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import type { AppConfig, RequestEntry } from '@/types'
import { Card, CardContent } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'

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

export default function LogsPage() {
  const s = useStrings()
  const [entries, setEntries] = useState<RequestEntry[]>([])
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [updatedAt, setUpdatedAt] = useState<Date | null>(null)

  useEffect(() => {
    api.getConfig().then(setConfig)
  }, [])

  useEffect(() => {
    let cancelled = false
    const load = () =>
      api.recentRequests(PAGE_SIZE).then((rows) => {
        if (cancelled) return
        setEntries(rows)
        setUpdatedAt(new Date())
      })
    load()
    const timer = setInterval(load, REFRESH_MS)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [])

  const providerName = useMemo(() => {
    const map = new Map<string, string>()
    for (const p of Object.values(config?.providers ?? {})) map.set(p.id, p.name)
    map.set('codex-native', s.overview.native)
    return (id: string) => map.get(id) ?? id
  }, [config, s])

  return (
    <div className="p-8 max-w-6xl">
      <div className="flex items-center justify-between mb-6">
        <div>
          <h2 className="text-2xl font-semibold tracking-tight">{s.logs.title}</h2>
          <p className="text-sm text-muted-foreground mt-1">
            {s.logs.subtitle}
            {updatedAt && ` · ${s.logs.updatedAt} ${fmtTime(updatedAt.getTime() / 1000)}`}
          </p>
        </div>
        <Badge variant="secondary" className="gap-1.5">
          <RefreshCw className="h-3 w-3" />
          {s.logs.autoRefresh}
        </Badge>
      </div>

      <Card>
        <CardContent className="p-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="pl-6">{s.logs.time}</TableHead>
                <TableHead>{s.logs.provider}</TableHead>
                <TableHead>{s.logs.model}</TableHead>
                <TableHead>{s.logs.status}</TableHead>
                <TableHead className="text-right">{s.logs.latency}</TableHead>
                <TableHead className="text-right">
                  <span className="inline-flex items-center gap-1">
                    <ArrowUpCircle className="h-3.5 w-3.5" />
                    {s.logs.input}
                  </span>
                </TableHead>
                <TableHead className="text-right">
                  <span className="inline-flex items-center gap-1">
                    <ArrowDownCircle className="h-3.5 w-3.5" />
                    {s.logs.output}
                  </span>
                </TableHead>
                <TableHead className="text-right pr-6">
                  <span className="inline-flex items-center gap-1">
                    <Database className="h-3.5 w-3.5" />
                    {s.logs.cached}
                  </span>
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {entries.map((e, i) => (
                <TableRow key={`${e.ts}-${i}`}>
                  <TableCell className="pl-6 font-mono text-xs text-muted-foreground">
                    {fmtTime(e.ts)}
                  </TableCell>
                  <TableCell>
                    <div className="text-sm">{providerName(e.provider)}</div>
                    <div className="text-xs text-muted-foreground uppercase">{e.transport}</div>
                  </TableCell>
                  <TableCell className="font-mono text-xs">{e.model}</TableCell>
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
                        {e.error && (
                          <p className="text-xs text-destructive mt-1 max-w-64 break-all">
                            {e.error}
                          </p>
                        )}
                      </div>
                    )}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs">
                    {fmtLatency(e.latency_ms)}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs">
                    {e.status === 'ok' ? fmt(e.input_tokens) : '—'}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs">
                    {e.status === 'ok' ? fmt(e.output_tokens) : '—'}
                  </TableCell>
                  <TableCell className="text-right font-mono text-xs pr-6">
                    {e.status === 'ok' ? fmt(e.cached_tokens) : '—'}
                  </TableCell>
                </TableRow>
              ))}
              {entries.length === 0 && (
                <TableRow>
                  <TableCell colSpan={8} className="pl-6 py-10 text-center">
                    <p className="text-sm text-muted-foreground">{s.logs.empty}</p>
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </CardContent>
      </Card>
    </div>
  )
}
