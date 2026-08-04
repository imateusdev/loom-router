import { useEffect, useState } from 'react'
import { CheckCircle2, XCircle } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import type { CodexStatus } from '@/types'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'

export default function CodexPage() {
  const s = useStrings()
  const [status, setStatus] = useState<CodexStatus | null>(null)
  const [busy, setBusy] = useState(false)

  const reload = () => api.codexStatus().then(setStatus)
  useEffect(() => {
    reload()
  }, [])

  const apply = async () => {
    setBusy(true)
    try {
      await api.codexApply()
      await reload()
    } finally {
      setBusy(false)
    }
  }

  const remove = async () => {
    setBusy(true)
    try {
      await api.codexRemove()
      await reload()
    } finally {
      setBusy(false)
    }
  }

  const active = status?.managed_block_present ?? false

  return (
    <div className="p-8 max-w-4xl">
      <h2 className="text-2xl font-semibold tracking-tight">{s.codex.title}</h2>
      <p className="text-sm text-muted-foreground mt-1 mb-6">{s.codex.subtitle}</p>

      <Card className="max-w-xl">
        <CardHeader className="flex-row items-center justify-between space-y-0">
          <CardTitle className="text-base">Codex</CardTitle>
          <Badge variant={active ? 'default' : 'secondary'}>
            {active ? s.codex.applied : s.codex.notApplied}
          </Badge>
        </CardHeader>
        <CardContent className="space-y-4">
          <StatusRow ok={status?.config_exists ?? false} label={s.codex.codexHome} detail={status?.codex_home} />
          <StatusRow ok={status?.codex_cli_available ?? false} label={s.codex.cliAvailable} />
          <StatusRow ok={status?.native_catalog_present ?? false} label={s.codex.nativeCatalog} />
          <StatusRow
            ok={status?.merged_catalog_present ?? false}
            label={s.codex.mergedCatalog}
            detail={s.codex.modelsInPicker.replace('{{count}}', String(status?.merged_model_count ?? 0))}
          />
          <div className="flex gap-2">
            <Button onClick={apply} disabled={busy}>
              {s.codex.apply}
            </Button>
            {active && (
              <Button variant="outline" onClick={remove} disabled={busy}>
                {s.codex.remove}
              </Button>
            )}
          </div>
          <p className="text-xs text-muted-foreground">{s.codex.restartHint}</p>
        </CardContent>
      </Card>
    </div>
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
