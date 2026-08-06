// First-run walkthrough: welcome -> connect Codex -> add a provider.
//
// Only the Codex step does any work; the provider step deliberately hands
// off to the real Providers page instead of duplicating its form, so there
// is one place where a provider is added and validated.
//
// The proxy is not started here — it already autostarts with the app (see
// `run()` in lib.rs), so by the time this renders the user is only choosing
// what to route through it.

import { useCallback, useEffect, useState } from 'react'
import { useNavigate } from 'react-router'
import { AlertTriangle, ArrowRight, Check, Loader2, Sparkles } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings } from '@/i18n'
import LanguageSwitcher from '@/components/LanguageSwitcher'
import { Button } from '@/components/ui/button'
import type { CodexStatus } from '@/types'
import logo from '@/assets/logo.png'

type Step = 'welcome' | 'codex' | 'providers'

/// Activation is a request plus a confirmation read, so the UI reports what
/// the backend actually ended up in rather than that the call returned.
type CodexPhase =
  | { kind: 'idle' }
  | { kind: 'working' }
  | { kind: 'active' }
  | { kind: 'failed'; message: string }

const STEP_COUNT = 2

export default function Onboarding({ onDone }: { onDone: () => void }) {
  const s = useStrings()
  const navigate = useNavigate()
  const [step, setStep] = useState<Step>('welcome')
  const [phase, setPhase] = useState<CodexPhase>({ kind: 'idle' })
  const [status, setStatus] = useState<CodexStatus | null>(null)
  const [port, setPort] = useState<number | null>(null)
  const [providerCount, setProviderCount] = useState(0)

  // Read once up front so the welcome copy can state the real port and the
  // provider step knows whether anything is configured already.
  useEffect(() => {
    let cancelled = false
    Promise.all([api.serverStatus(), api.getConfig(), api.codexStatus()])
      .then(([server, config, codex]) => {
        if (cancelled) return
        setPort(server.port)
        setProviderCount(Object.keys(config.providers ?? {}).length)
        setStatus(codex)
        // Already applied (e.g. the user reopened mid-walkthrough): show it
        // as done instead of asking them to activate it twice.
        if (codex.managed_block_present) setPhase({ kind: 'active' })
      })
      .catch(() => {
        // Nothing here is required to continue; the steps still work.
      })
    return () => {
      cancelled = true
    }
  }, [])

  const activateCodex = useCallback(async () => {
    setPhase({ kind: 'working' })
    try {
      await api.codexApply()
      const fresh = await api.codexStatus()
      setStatus(fresh)
      setPhase(
        fresh.managed_block_present
          ? { kind: 'active' }
          : { kind: 'failed', message: s.onboarding.codexFailed },
      )
    } catch (e) {
      setPhase({ kind: 'failed', message: e instanceof Error ? e.message : String(e) })
    }
  }, [s])

  // Persist first, then leave: if the write fails the walkthrough should run
  // again rather than strand the user in a half-finished setup.
  const finish = useCallback(
    async (to: string) => {
      try {
        await api.completeOnboarding()
      } catch {
        // Best effort — never trap the user behind a failed write.
      }
      onDone()
      navigate(to)
    },
    [navigate, onDone],
  )

  return (
    <div className="flex h-screen flex-col bg-background text-foreground">
      <div className="flex justify-end p-3">
        <LanguageSwitcher />
      </div>
      <div className="flex flex-1 items-center justify-center px-6 pb-16">
        <div className="w-full max-w-lg">
          {step === 'welcome' && (
            <Welcome port={port} onStart={() => setStep('codex')} />
          )}

          {step !== 'welcome' && (
            <p className="mb-3 text-xs font-medium uppercase tracking-wide text-muted-foreground">
              {s.onboarding.stepOf
                .replace('{{current}}', String(step === 'codex' ? 1 : 2))
                .replace('{{total}}', String(STEP_COUNT))}
            </p>
          )}

          {step === 'codex' && (
            <section className="rounded-xl border border-border p-6">
              <h2 className="text-xl font-semibold tracking-tight">{s.onboarding.codexTitle}</h2>
              <p className="mt-2 text-sm text-muted-foreground">
                {s.onboarding.codexDescription}
              </p>

              {status && !status.codex_cli_available && phase.kind !== 'active' && (
                <Notice tone="warning" icon={AlertTriangle}>
                  {s.onboarding.codexCliMissing}
                </Notice>
              )}

              {phase.kind === 'active' && (
                <Notice tone="success" icon={Check}>
                  <span className="font-medium">{s.onboarding.codexActive}</span>
                  <span className="block text-muted-foreground">
                    {s.onboarding.codexActiveHint}
                  </span>
                </Notice>
              )}

              {phase.kind === 'failed' && (
                <Notice tone="danger" icon={AlertTriangle}>
                  {phase.message}
                </Notice>
              )}

              <div className="mt-6 flex items-center gap-3">
                {phase.kind !== 'active' && (
                  <Button onClick={activateCodex} disabled={phase.kind === 'working'}>
                    {phase.kind === 'working' && (
                      <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                    )}
                    {phase.kind === 'working'
                      ? s.onboarding.codexActivating
                      : phase.kind === 'failed'
                        ? s.onboarding.retry
                        : s.onboarding.codexActivate}
                  </Button>
                )}
                <Button
                  variant={phase.kind === 'active' ? 'default' : 'ghost'}
                  onClick={() => setStep('providers')}
                >
                  {phase.kind === 'active' ? s.onboarding.next : s.onboarding.skip}
                  <ArrowRight className="ml-2 h-4 w-4" />
                </Button>
              </div>
            </section>
          )}

          {step === 'providers' && (
            <section className="rounded-xl border border-border p-6">
              <h2 className="text-xl font-semibold tracking-tight">
                {s.onboarding.providersTitle}
              </h2>
              <p className="mt-2 text-sm text-muted-foreground">
                {s.onboarding.providersDescription}
              </p>

              {providerCount > 0 && (
                <Notice tone="success" icon={Check}>
                  {s.onboarding.providersConfigured.replace('{{count}}', String(providerCount))}
                </Notice>
              )}

              <div className="mt-6 flex items-center gap-3">
                <Button onClick={() => finish('/providers')}>
                  {s.onboarding.providersGo}
                  <ArrowRight className="ml-2 h-4 w-4" />
                </Button>
                <Button variant="ghost" onClick={() => finish('/')}>
                  {providerCount > 0 ? s.onboarding.finish : s.onboarding.skip}
                </Button>
              </div>

              <button
                onClick={() => setStep('codex')}
                className="mt-5 text-xs text-muted-foreground underline-offset-4 hover:underline"
              >
                {s.onboarding.back}
              </button>
            </section>
          )}
        </div>
      </div>
    </div>
  )
}

function Welcome({ port, onStart }: { port: number | null; onStart: () => void }) {
  const s = useStrings()
  return (
    <section className="text-center">
      <img src={logo} alt="" className="mx-auto h-16 w-16 rounded-2xl" />
      <h1 className="mt-6 text-2xl font-semibold tracking-tight">{s.onboarding.welcomeTitle}</h1>
      <p className="mx-auto mt-3 max-w-md text-sm text-muted-foreground">
        {s.onboarding.welcomeSubtitle}
      </p>
      {port !== null && (
        <p className="mt-4 inline-flex items-center gap-2 rounded-full border border-border px-3 py-1 text-xs text-muted-foreground">
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
    </section>
  )
}

/// Small inline status block. Tones map to the same palette the rest of the
/// app uses for the equivalent states.
function Notice({
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
