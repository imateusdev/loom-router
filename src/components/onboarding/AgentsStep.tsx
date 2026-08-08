import { AlertTriangle } from 'lucide-react'
import { useStrings } from '@/i18n'
import { Button } from '@/components/ui/button'
import { Switch } from '@/components/ui/switch'
import { Notice, StepBack } from './shared'

export function AgentsStep({
  multiAgent,
  busy,
  error,
  onToggle,
  onFinish,
  onBack,
}: {
  multiAgent: boolean | null
  busy: boolean
  error: string | null
  onToggle: (next: boolean) => void
  onFinish: () => void
  onBack: () => void
}) {
  const s = useStrings()
  return (
    <section className="rounded-xl border border-border p-6">
      <h2 className="text-xl font-semibold tracking-tight">{s.onboarding.agentsTitle}</h2>
      <p className="mt-2 text-sm text-muted-foreground">
        {s.onboarding.agentsDescription}
      </p>

      <label className="mt-5 flex items-center gap-3 rounded-lg border border-border p-3 text-sm">
        <Switch
          checked={multiAgent ?? false}
          onCheckedChange={onToggle}
          disabled={busy || multiAgent === null}
          aria-label={s.onboarding.agentsMultiAgent}
        />
        <span className="min-w-0">
          <span className="font-medium">{s.onboarding.agentsMultiAgent}</span>
          <span className="block text-muted-foreground">{s.onboarding.agentsMultiAgentHint}</span>
        </span>
      </label>

      {error && (
        <Notice tone="danger" icon={AlertTriangle}>
          {error}
        </Notice>
      )}

      <div className="mt-6 flex items-center gap-3">
        <Button onClick={onFinish} disabled={busy}>
          {s.onboarding.finish}
        </Button>
      </div>
      <StepBack onClick={onBack} />
    </section>
  )
}
