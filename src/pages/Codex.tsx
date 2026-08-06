import { useEffect, useState } from 'react'
import { CheckCircle2, XCircle } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import type { AppConfig, CodexStatus } from '@/types'
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

  const reload = () => api.codexStatus().then(setStatus)
  useEffect(() => {
    reload()
    api.getConfig().then(setConfig)
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

      <div className="mt-6 grid gap-6 max-w-xl">
        <SideCallCard config={config} onChanged={setConfig} />
        <NativeSlugCard config={config} onChanged={setConfig} onReload={reload} />
        <MultiAgentCard />
      </div>
    </div>
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
