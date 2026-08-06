import { useEffect, useState } from 'react'
import { CheckCircle2, XCircle } from 'lucide-react'
import { api } from '@/lib/api'
import { useBackendState } from '@/lib/events'
import { useStrings } from '@/i18n'
import type { AppConfig, CodexStatus } from '@/types'
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

export default function CodexPage() {
  const s = useStrings()
  const [status, setStatus] = useState<CodexStatus | null>(null)
  const [config, setConfig] = useState<AppConfig | null>(null)
  const [busy, setBusy] = useState(false)

  const reload = () => {
    api.getConfig().then(setConfig)
    return api.codexStatus().then(setStatus)
  }
  useEffect(() => {
    reload()
  }, [])
  // The tray applies and removes the integration too.
  useBackendState(reload)

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
    <PageShell title={s.codex.title} subtitle={s.codex.subtitle}>
      {/* The status card stays full-measure — it is the page's subject and
          its rows are long. The settings below it flow in the shared grid. */}
      <Card className="mb-6">
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
          <p className="text-xs text-muted-foreground">{s.codex.restartHint}</p>
          {status?.integration_enabled && (
            <p className="text-xs text-green-600 dark:text-green-500">{s.codex.autoApplyHint}</p>
          )}
        </CardContent>
      </Card>

      {/* A fixed trio, so a tighter track than the shared CARD_GRID: at 340px
          the third card was orphaned on its own row from 992px through 1371px,
          which includes the default 1100x760 window. */}
      <div className="grid grid-cols-[repeat(auto-fit,minmax(240px,1fr))] items-start gap-6">
        <SideCallCard config={config} onChanged={setConfig} />
        <NativeSlugCard config={config} onChanged={setConfig} onReload={reload} />
        <MultiAgentCard />
      </div>
    </PageShell>
  )
}

function SideCallCard({
  config,
  onChanged,
}: {
  config: AppConfig | null
  onChanged: (config: AppConfig) => void
}) {
  const s = useStrings()
  const [busy, setBusy] = useState(false)
  const value = config?.side_call_fallback ?? OFF_SENTINEL

  // Enabled models across all providers, as "provider/model" slugs.
  const models = config
    ? Object.values(config.providers).flatMap((p) =>
        p.models.filter((m) => m.enabled).map((m) => `${p.id}/${m.id}`),
      )
    : []
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
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{s.codex.sideCallTitle}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">{s.codex.sideCallDescription}</p>
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
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{s.codex.nativeSlugTitle}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">{s.codex.nativeSlugDescription}</p>
        <label className="flex items-center gap-3 text-sm">
          <Switch checked={enabled} onCheckedChange={change} disabled={busy || !config} />
          <span>{enabled ? s.common.on : s.common.off}</span>
        </label>
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
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{s.codex.multiAgentTitle}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <p className="text-sm text-muted-foreground">{s.codex.multiAgentDescription}</p>
        <label className="flex items-center gap-3 text-sm">
          <Switch
            checked={enabled ?? false}
            onCheckedChange={change}
            disabled={busy || enabled === null}
          />
          <span>{enabled ? s.common.on : s.common.off}</span>
        </label>
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
