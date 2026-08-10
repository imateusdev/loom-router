import { useState } from 'react'
import { ArrowRight, ChevronDown, Sparkles } from 'lucide-react'
import { useStrings, type Strings } from '@/i18n'
import logo from '@/assets/logo.png'
import { Button } from '@/components/ui/button'

type OnboardingKey = keyof Strings['onboarding']

const TERMS: Array<{ key: string; label: OnboardingKey; hint: OnboardingKey }> = [
  { key: 'provider', label: 'termProvider', hint: 'termProviderHint' },
  { key: 'api-key', label: 'termApiKey', hint: 'termApiKeyHint' },
  { key: 'model', label: 'termModel', hint: 'termModelHint' },
  { key: 'proxy', label: 'termProxy', hint: 'termProxyHint' },
  { key: 'integration', label: 'termIntegration', hint: 'termIntegrationHint' },
]

export function Welcome({ port, onStart }: { port: number | null; onStart: () => void }) {
  const s = useStrings()
  const [openTerm, setOpenTerm] = useState<string | null>(null)

  return (
    <section className="text-center">
      <img src={logo} alt="" className="mx-auto h-16 w-16 rounded-2xl" />
      <h1 className="mt-6 text-2xl font-semibold tracking-tight">{s.app.name}</h1>
      <p className="mx-auto mt-3 max-w-md text-sm text-muted-foreground">
        {s.onboarding.welcomeSubtitle}
      </p>
      {port !== null && (
        <p className="mt-4 inline-flex items-center gap-2 rounded-full border border-border px-3 py-1 text-xs text-muted-foreground">
          <Sparkles className="h-3 w-3" />
          {s.onboarding.welcomeProxyReady.replace('{{port}}', String(port))}
        </p>
      )}

      <div className="mx-auto mt-6 max-w-md space-y-2 text-left">
        {TERMS.map((term) => {
          const open = openTerm === term.key
          return (
            <div key={term.key} className="rounded-lg border border-border">
              <button
                type="button"
                aria-expanded={open}
                aria-controls={`term-${term.key}`}
                onClick={() => setOpenTerm(open ? null : term.key)}
                className="flex w-full items-center justify-between gap-3 px-3 py-2 text-left text-sm font-medium"
              >
                <span>{s.onboarding[term.label]}</span>
                <ChevronDown className={`h-4 w-4 shrink-0 transition-transform ${open ? 'rotate-180' : ''}`} />
              </button>
              {open && (
                <p id={`term-${term.key}`} className="border-t border-border px-3 py-3 text-sm text-muted-foreground">
                  {s.onboarding[term.hint]}
                </p>
              )}
            </div>
          )
        })}
      </div>

      <div className="mt-8">
        <Button size="lg" onClick={onStart}>
          {s.onboarding.start}
          <ArrowRight className="ml-2 h-4 w-4" />
        </Button>
      </div>
    </section>
  )
}
