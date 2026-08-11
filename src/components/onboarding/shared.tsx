import { Check } from 'lucide-react'
import { useStrings } from '@/i18n'

export function StepBack({ onClick }: { onClick: () => void }) {
  const s = useStrings()
  return (
    <button
      onClick={onClick}
      className="mt-5 text-xs text-muted-foreground underline-offset-4 hover:underline"
    >
      {s.onboarding.back}
    </button>
  )
}

export function Notice({
  tone,
  icon: Icon,
  children,
}: {
  tone: 'success' | 'warning' | 'danger'
  icon: typeof Check
  children: React.ReactNode
}) {
  const tones = {
    success: 'border-emerald-500/30 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400',
    warning: 'border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400',
    danger: 'border-destructive/30 bg-destructive/10 text-destructive',
  }
  return (
    <div className={`mt-5 flex gap-2 rounded-lg border p-3 text-sm ${tones[tone]}`}>
      <Icon className="mt-0.5 h-4 w-4 shrink-0" />
      <div className="min-w-0">{children}</div>
    </div>
  )
}
