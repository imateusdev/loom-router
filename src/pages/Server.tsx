import { useEffect, useState } from 'react'
import { Play, Square } from 'lucide-react'
import { api } from '@/lib/api'
import { useBackendState } from '@/lib/events'
import { useStrings } from '@/i18n'
import type { ServerStatus } from '@/types'
import PageShell from '@/components/PageShell'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'

export default function ServerPage() {
  const s = useStrings()
  const [status, setStatus] = useState<ServerStatus | null>(null)

  const reload = () => api.serverStatus().then(setStatus)
  useEffect(() => {
    reload()
  }, [])
  // The tray can start or stop the proxy while this page is open.
  useBackendState(reload)

  const start = async () => setStatus(await api.serverStart())
  const stop = async () => setStatus(await api.serverStop())

  return (
    <PageShell title={s.server.title} subtitle={s.server.subtitle}>
      {/* One card, so it keeps a readable measure instead of stretching a
          handful of key/value rows across the whole window. */}
      <Card className="max-w-xl">
        <CardHeader className="flex-row items-center justify-between space-y-0">
          <CardTitle className="text-base">LoomRouter proxy</CardTitle>
          <Badge variant={status?.running ? 'default' : 'secondary'}>
            {status?.running ? s.server.running : s.server.stopped}
          </Badge>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="text-sm">
            <span className="text-muted-foreground">{s.server.port}: </span>
            <code className="font-mono">{status?.port ?? '-'}</code>
          </div>
          {status?.url && (
            <div className="text-sm">
              <span className="text-muted-foreground">{s.server.listeningOn}: </span>
              <code className="font-mono">{status.url}</code>
            </div>
          )}
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
    </PageShell>
  )
}
