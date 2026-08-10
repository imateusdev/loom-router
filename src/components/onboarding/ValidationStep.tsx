import { AlertTriangle, ArrowRight, Check, ExternalLink, RefreshCw, Sparkles } from 'lucide-react'
import { useStrings } from '@/i18n'
import { Button } from '@/components/ui/button'
import type { SetupStatus } from '@/types'
import { Notice, StepBack } from './shared'

export function ValidationStep({
  setup,
  onBack,
  onCheck,
  onFinish,
  onLogs,
  onSkip,
}: {
  setup: SetupStatus | null
  onBack: () => void
  onCheck: () => void
  onFinish: () => void
  onLogs: () => void
  onSkip: () => void
}) {
  const s = useStrings()
  const firstOk = setup?.validation.first_ok_request_at != null
  const failed = setup?.validation.failed_attempt === true
  const ready = setup?.ready === true
  const missing = setup?.missing ?? []
  const missingText = missing
    .map((item) => {
      if (item === 'codex_integration') return s.onboarding.missingCodex
      if (item === 'provider') return s.onboarding.missingProvider
      return s.onboarding.missingModel
    })
    .join(', ')

  return (
    <section className="rounded-xl border border-border p-6">
      <h2 className="text-xl font-semibold tracking-tight">{s.onboarding.validationTitle}</h2>
      <p className="mt-2 text-sm text-muted-foreground">
        {s.onboarding.validationDescription}
      </p>

      {!ready ? (
        <Notice tone="warning" icon={AlertTriangle}>
          <span className="font-medium">{s.onboarding.validationMissing}</span>
          {missingText && <span className="block text-muted-foreground">{missingText}</span>}
        </Notice>
      ) : firstOk ? (
        <Notice tone="success" icon={Check}>
          <span className="font-medium">{s.onboarding.validationSuccess}</span>
          <span className="block text-muted-foreground">{s.onboarding.validationSuccessHint}</span>
        </Notice>
      ) : failed ? (
        <Notice tone="danger" icon={AlertTriangle}>
          <span className="font-medium">{s.onboarding.validationFailed}</span>
          <span className="block text-muted-foreground">{s.onboarding.validationFailedHint}</span>
        </Notice>
      ) : (
        <Notice tone="success" icon={Sparkles}>
          <span className="font-medium">{s.onboarding.validationReady}</span>
          <span className="block text-muted-foreground">{s.onboarding.validationFirstRequest}</span>
        </Notice>
      )}

      <div className="mt-6 flex flex-wrap items-center gap-3">
        <Button variant="outline" onClick={onCheck}>
          <RefreshCw className="h-4 w-4" />
          {s.onboarding.validationCheckAgain}
        </Button>
        {failed && (
          <Button variant="outline" onClick={onLogs}>
            <ExternalLink className="h-4 w-4" />
            {s.onboarding.validationOpenLogs}
          </Button>
        )}
        <Button variant="ghost" onClick={onSkip}>
          {s.onboarding.next}
          <ArrowRight className="ml-2 h-4 w-4" />
        </Button>
        <Button onClick={onFinish}>{s.onboarding.finishLater}</Button>
      </div>
      <StepBack onClick={onBack} />
    </section>
  )
}
