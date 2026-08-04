import { useEffect, useState } from 'react'
import { Play, Square } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import type { ServerStatus } from '@/types'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'

export default function ServerPage() {
  const s = useStrings()
  const [status, setStatus] = useState<ServerStatus | null>(null)

  useEffect(() => {
    api.serverStatus().then(setStatus)
  }, [])

  const start = async () => setStatus(await api.serverStart())
  const stop = async () => setStatus(await api.serverStop())

  return (
    <div className="p-8 max-w-4xl">
      <h2 className="text-2xl font-semibold tracking-tight">{s.server.title}</h2>
      <p className="text-sm text-muted-foreground mt-1 mb-6">{s.server.subtitle}</p>

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
            <code className="font-mono">{status?.port ?? '—'}</code>
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
    </div>
  )
}
