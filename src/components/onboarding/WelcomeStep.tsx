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
    <section className="grid gap-8 md:grid-cols-[minmax(0,0.9fr)_minmax(0,1.1fr)] md:items-center">
      <div className="min-w-0">
        <img src={logo} alt="" className="h-12 w-12 rounded-xl" />
        <h1 className="mt-5 text-3xl font-semibold tracking-tight text-balance md:text-4xl">
          {s.app.name}
        </h1>
        <p className="mt-3 max-w-md text-base leading-6 text-muted-foreground text-pretty">
          {s.onboarding.welcomeSubtitle}
        </p>
        {port !== null && (
          <p className="mt-5 inline-flex items-center gap-2 rounded-full border border-border px-3 py-1 text-xs text-muted-foreground">
            <Sparkles className="h-3 w-3" />
            {s.onboarding.welcomeProxyReady.replace('{{port}}', String(port))}
          </p>
        )}

        <div className="mt-8">
          <Button size="lg" onClick={onStart}>
            {s.onboarding.start}
            <ArrowRight className="ml-2 h-4 w-4" />
          </Button>
        </div>
      </div>

      <div className="rounded-xl border border-border bg-muted/40 p-5">
        <h2 className="text-sm font-semibold">{s.onboarding.welcomeSetupTitle}</h2>
        <div className="mt-4 space-y-2">
          {TERMS.map((term) => {
            const open = openTerm === term.key
            return (
              <div key={term.key} className="rounded-lg border border-border bg-background">
                <button
                  type="button"
                  aria-expanded={open}
                  aria-controls={`term-${term.key}`}
                  onClick={() => setOpenTerm(open ? null : term.key)}
                  className="flex min-h-[44px] w-full items-center justify-between gap-3 px-3 py-2 text-left text-sm font-medium"
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
      </div>
    </section>
  )
}
