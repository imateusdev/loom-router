import { useStrings } from '@/i18n'
import { Button } from '@/components/ui/button'
import { StepBack } from './shared'

export function FinishStep({
  onFinish,
  onBack,
}: {
  onFinish: () => void
  onBack: () => void
}) {
  const s = useStrings()
  return (
    <section className="rounded-xl border border-border p-6">
      <h2 className="text-xl font-semibold tracking-tight">{s.onboarding.finishTitle}</h2>
      <p className="mt-2 text-sm text-muted-foreground">{s.onboarding.finishDescription}</p>
      <div className="mt-6 flex items-center gap-3">
        <Button onClick={onFinish}>{s.onboarding.finish}</Button>
      </div>
      <StepBack onClick={onBack} />
    </section>
  )
}
