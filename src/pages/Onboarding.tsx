// First-run walkthrough: welcome, Codex, detect, provider.
//
// The later wizard states (validation, agents, finish) are represented by a
// placeholder here; task_04 replaces that placeholder with the real flows.

import { useCallback, useEffect, useRef, useState } from 'react'
import { useNavigate } from 'react-router'
import { AlertTriangle, ArrowRight, Check, ExternalLink, Loader2, Sparkles } from 'lucide-react'
import { api } from '@/lib/api'
import { useStrings, type Strings } from '@/i18n'
import LanguageSwitcher from '@/components/LanguageSwitcher'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { PRESETS, type CodexStatus, type Provider, type SetupStatus, type ToolDetection, type WizardStep } from '@/types'
import { Welcome } from '@/components/onboarding/WelcomeStep'
import { ValidationStep } from '@/components/onboarding/ValidationStep'
import { AgentsStep } from '@/components/onboarding/AgentsStep'
import { FinishStep } from '@/components/onboarding/FinishStep'
import { Notice, StepBack } from '@/components/onboarding/shared'

type Step = WizardStep

type CodexPhase =
  | { kind: 'idle' }
  | { kind: 'working' }
  | { kind: 'active' }
  | { kind: 'failed'; message: string }

type GatewayId = 'opencode-zen' | 'opencode-go' | 'claude-code'

const STEP_ORDER: Step[] = ['welcome', 'codex', 'detect', 'provider', 'validate', 'agents', 'finish']
const TOTAL_STEPS = STEP_ORDER.length - 2
const REFRESH_MS = 5000

const WIZARD_PRESETS = PRESETS.filter(
  (preset) => preset.id !== 'claude-code' && !preset.id.startsWith('opencode'),
)

const KEY_URLS: Record<string, string> = {
  openrouter: 'https://openrouter.ai/keys',
  'kimi-coding': 'https://platform.moonshot.ai/console/keys',
  deepseek: 'https://platform.deepseek.com/api_keys',
  anthropic: 'https://console.anthropic.com/settings/keys',
  groq: 'https://console.groq.com/keys',
  together: 'https://api.together.ai/settings/api-keys',
  mistral: 'https://console.mistral.ai/api-keys',
  siliconflow: 'https://cloud.siliconflow.cn/account/ak',
  'zai-coding': 'https://open.bigmodel.cn/usercenter/apikeys',
  'moonshot-global': 'https://platform.moonshot.ai/console/keys',
  'moonshot-cn': 'https://platform.moonshot.cn/console/keys',
}

type OnboardingKey = keyof Strings['onboarding']

const PRESET_HINTS: Record<string, OnboardingKey> = {
  openrouter: 'providerHintOpenrouter',
  'kimi-coding': 'providerHintKimiCoding',
  deepseek: 'providerHintDeepseek',
  anthropic: 'providerHintAnthropic',
  groq: 'providerHintGroq',
  together: 'providerHintTogether',
  mistral: 'providerHintMistral',
  siliconflow: 'providerHintSiliconflow',
  'zai-coding': 'providerHintZaiCoding',
  'moonshot-global': 'providerHintMoonshotGlobal',
  'moonshot-cn': 'providerHintMoonshotCn',
}

export default function Onboarding({ onDone }: { onDone: () => void }) {
  const s = useStrings()
  const navigate = useNavigate()
  const [step, setStep] = useState<Step>('welcome')
  const [phase, setPhase] = useState<CodexPhase>({ kind: 'idle' })
  const [status, setStatus] = useState<CodexStatus | null>(null)
  const [port, setPort] = useState<number | null>(null)
  const [setup, setSetup] = useState<SetupStatus | null>(null)
  const [detection, setDetection] = useState<ToolDetection | null>(null)
  const [confirming, setConfirming] = useState<GatewayId | null>(null)
  const [importing, setImporting] = useState(false)
  const [detectError, setDetectError] = useState<string | null>(null)
  const [providerId, setProviderId] = useState('')
  const [recommended, setRecommended] = useState(false)
  const [custom, setCustom] = useState(false)
  const [customName, setCustomName] = useState('')
  const [customBaseUrl, setCustomBaseUrl] = useState('')
  const [apiKey, setApiKey] = useState('')
  const [keyFocused, setKeyFocused] = useState(false)
  const [validating, setValidating] = useState(false)
  const [providerError, setProviderError] = useState<string | null>(null)
  const [savedProvider, setSavedProvider] = useState<Provider | null>(null)
  const [multiAgent, setMultiAgent] = useState<boolean | null>(null)
  const [multiAgentBusy, setMultiAgentBusy] = useState(false)
  const [multiAgentError, setMultiAgentError] = useState<string | null>(null)
  const codexBusy = useRef(false)
  const setupProbeInFlight = useRef(false)
  const keyInputRef = useRef<HTMLInputElement>(null)
  const contentRef = useRef<HTMLDivElement>(null)

  const reloadSetup = useCallback(async () => {
    if (setupProbeInFlight.current) return
    setupProbeInFlight.current = true
    try {
      setSetup(await api.setupStatus())
    } catch {
      setSetup(null)
    } finally {
      setupProbeInFlight.current = false
    }
  }, [])

  const reloadDetection = useCallback(async () => {
    try {
      setDetection(await api.detectTools())
      setDetectError(null)
    } catch (e) {
      setDetection(null)
      setDetectError(e instanceof Error ? e.message : String(e))
    }
  }, [])

  useEffect(() => {
    let cancelled = false
    void Promise.allSettled([
      api.getConfig(),
      api.serverStatus(),
      api.codexStatus(),
      api.detectTools(),
      api.setupStatus(),
      api.multiAgentStatus(),
    ]).then(([configResult, server, codex, tools, setupResult, multiAgentResult]) => {
      if (cancelled) return
      if (configResult.status === 'fulfilled') {
        const persisted = configResult.value.onboarding_step
        if (persisted && STEP_ORDER.includes(persisted) && persisted !== 'welcome') {
          setStep(persisted)
        }
      }
      if (server.status === 'fulfilled') setPort(server.value.port)
      if (codex.status === 'fulfilled') {
        setStatus(codex.value)
        if (codex.value.managed_block_present && codex.value.integration_enabled) {
          setPhase({ kind: 'active' })
        }
      }
      if (tools.status === 'fulfilled') setDetection(tools.value)
      if (setupResult.status === 'fulfilled') setSetup(setupResult.value)
      if (multiAgentResult.status === 'fulfilled') setMultiAgent(multiAgentResult.value)
    })
    return () => {
      cancelled = true
    }
  }, [])

  const goTo = useCallback(async (next: Step) => {
    if (next === 'welcome') {
      setStep('welcome')
      return
    }
    try {
      await api.setOnboardingStep(next)
      setStep(next)
      setConfirming(null)
    } catch {
      // Resume from the last persisted step when the write fails.
    }
  }, [])

  const activateCodex = useCallback(async () => {
    if (codexBusy.current || phase.kind === 'working') return
    codexBusy.current = true
    setPhase({ kind: 'working' })
    try {
      await api.codexApply()
      const fresh = await api.codexStatus()
      setStatus(fresh)
      setPhase(
        fresh.managed_block_present && fresh.integration_enabled
          ? { kind: 'active' }
          : { kind: 'failed', message: s.onboarding.codexFailed },
      )
    } catch (e) {
      setPhase({ kind: 'failed', message: e instanceof Error ? e.message : String(e) })
    } finally {
      codexBusy.current = false
      void reloadSetup()
    }
  }, [phase.kind, reloadSetup, s])

  const finish = useCallback(
    async (to: string) => {
      try {
        await api.completeOnboarding()
      } catch {
        // Best effort - never trap the user behind a failed write.
      }
      onDone()
      navigate(to)
    },
    [navigate, onDone],
  )

  useEffect(() => {
    if (step !== 'validate') return
    let timer: ReturnType<typeof setInterval> | undefined
    const start = () => {
      void reloadSetup()
      timer = setInterval(() => void reloadSetup(), REFRESH_MS)
    }
    const stop = () => {
      if (timer !== undefined) {
        clearInterval(timer)
        timer = undefined
      }
    }
    const onVisibility = () => {
      if (document.hidden) stop()
      else start()
    }
    if (!document.hidden) start()
    document.addEventListener('visibilitychange', onVisibility)
    return () => {
      stop()
      document.removeEventListener('visibilitychange', onVisibility)
    }
  }, [reloadSetup, step])

  useEffect(() => {
    const heading = contentRef.current?.querySelector<HTMLElement>('h1, h2')
    if (heading) {
      heading.tabIndex = -1
      heading.focus()
    }
  }, [step])

  const toggleMultiAgent = useCallback(async (next: boolean) => {
    setMultiAgentBusy(true)
    setMultiAgentError(null)
    try {
      setMultiAgent(await api.setMultiAgent(next))
    } catch {
      setMultiAgentError(s.onboarding.agentsWriteFailed)
    } finally {
      setMultiAgentBusy(false)
    }
  }, [s])

  const buildProvider = (): Provider | null => {
    if (custom) {
      const id = customName.trim().toLowerCase().replace(/[^a-z0-9]+/g, '-') || 'custom'
      return {
        id,
        name: customName.trim() || 'Custom',
        protocol: 'openai',
        base_url: customBaseUrl.trim(),
        api_key: apiKey || null,
        has_key: false,
        user_agent: null,
        models: [],
        enabled: true,
      }
    }
    const preset = PRESETS.find((candidate) => candidate.id === providerId)
    if (!preset) return null
    return {
      id: preset.id,
      name: preset.name,
      protocol: preset.protocol,
      base_url: preset.base_url,
      api_key: apiKey || null,
      has_key: false,
      user_agent: preset.userAgent ?? null,
      models: (preset.defaultModels ?? []).map((model) =>
        typeof model === 'string'
          ? { id: model, enabled: true, supports_vision: false }
          : { id: model[0], protocol: model[1], enabled: true, supports_vision: false },
      ),
      enabled: true,
    }
  }

  const saveProvider = async (skipValidation = false) => {
    const built = buildProvider()
    if (!built) return
    if (!apiKey.trim()) {
      setProviderError(s.onboarding.providerKeyRequired)
      keyInputRef.current?.focus()
      return
    }
    setValidating(true)
    setProviderError(null)
    try {
      if (!skipValidation) {
        const ids = await api.validateProvider(built)
        if (ids.length > 0) {
          const existing = new Map(built.models.map((model) => [model.id, model]))
          built.models = ids.map((id, index) => existing.get(id) ?? {
            id,
            enabled: index === 0,
            supports_vision: false,
          })
        } else {
          setProviderError(s.onboarding.providerNoModels)
          setValidating(false)
          return
        }
      }
      await api.saveProvider(built)
      const nextConfig = await api.getConfig()
      setSavedProvider(nextConfig.providers[built.id] ?? built)
      setApiKey('')
      setProviderError(null)
      void reloadSetup()
    } catch (e) {
      setProviderError(`${s.onboarding.providerValidationFailed}: ${String(e)}`)
      keyInputRef.current?.focus()
    } finally {
      setValidating(false)
    }
  }

  const saveAnyway = async () => {
    const built = buildProvider()
    if (!built) return
    if (!apiKey.trim()) {
      setProviderError(s.onboarding.providerKeyRequired)
      keyInputRef.current?.focus()
      return
    }
    setValidating(true)
    setProviderError(null)
    try {
      await api.saveProvider(built)
      const nextConfig = await api.getConfig()
      setSavedProvider(nextConfig.providers[built.id] ?? built)
      setApiKey('')
      void reloadSetup()
    } catch (e) {
      setProviderError(e instanceof Error ? e.message : String(e))
    } finally {
      setValidating(false)
    }
  }

  const openKeyLink = () => {
    const url = KEY_URLS[providerId]
    if (!url) return
    try {
      const opened = window.open(url, '_blank', 'noopener,noreferrer')
      if (opened == null) setProviderError(s.onboarding.providerKeyLinkFailed)
    } catch {
      setProviderError(s.onboarding.providerKeyLinkFailed)
    }
  }

  const selectProvider = (value: string) => {
    if (value === '__recommend') {
      setProviderId('openrouter')
      setRecommended(true)
    } else if (value === '__none') {
      setProviderId('')
      setRecommended(false)
    } else {
      setProviderId(value)
      setRecommended(false)
    }
  }

  const toggleSavedModel = async (modelId: string, enabled: boolean) => {
    if (!savedProvider) return
    setSavedProvider((prev) =>
      prev
        ? {
            ...prev,
            models: prev.models.map((model) =>
              model.id === modelId ? { ...model, enabled } : model,
            ),
          }
        : prev,
    )
    try {
      await api.toggleModel(savedProvider.id, modelId, enabled)
      const nextConfig = await api.getConfig()
      setSavedProvider(nextConfig.providers[savedProvider.id] ?? null)
      void reloadSetup()
    } catch {
      const nextConfig = await api.getConfig()
      setSavedProvider(nextConfig.providers[savedProvider.id] ?? null)
    }
  }

  const confirmImport = async () => {
    if (!confirming || importing) return
    setImporting(true)
    setDetectError(null)
    try {
      if (confirming === 'claude-code') await api.importClaudeCode()
      else await api.importOpencodeGateway(confirming)
      setConfirming(null)
      await Promise.all([reloadDetection(), reloadSetup()])
    } catch (e) {
      setDetectError(e instanceof Error ? e.message : String(e))
    } finally {
      setImporting(false)
    }
  }

  const selectedProvider = WIZARD_PRESETS.find((preset) => preset.id === providerId) ?? null
  const stepIndex = STEP_ORDER.indexOf(step)
  const codexActive =
    phase.kind === 'active' ||
    Boolean(status?.managed_block_present && status.integration_enabled)

  return (
    <div className="flex h-screen flex-col bg-background text-foreground">
      <div className="flex justify-end p-3">
        <div className="w-48 shrink-0">
          <LanguageSwitcher />
        </div>
      </div>
      <div className="flex min-h-0 flex-1 items-start justify-center overflow-y-auto px-6 py-8">
        <div ref={contentRef} aria-live="polite" className={`w-full ${step === 'welcome' ? 'max-w-4xl' : 'max-w-2xl'}`}>
          {step === 'welcome' && (
            <Welcome port={port} onStart={() => void goTo('codex')} />
          )}

          {step !== 'welcome' && (
            <p className="mb-3 text-xs font-medium uppercase tracking-wide text-muted-foreground">
              {s.onboarding.stepOf
                .replace('{{current}}', String(stepIndex))
                .replace('{{total}}', String(TOTAL_STEPS))}
            </p>
          )}

          {step === 'codex' && (
            <section className="rounded-xl border border-border p-6">
              <h2 className="text-xl font-semibold tracking-tight">{s.onboarding.codexTitle}</h2>
              <p className="mt-2 text-sm text-muted-foreground">
                {s.onboarding.codexDescription}
              </p>

              {status && !status.codex_cli_available && !codexActive && (
                <Notice tone="warning" icon={AlertTriangle}>
                  {s.onboarding.codexCliMissing}
                </Notice>
              )}

              {codexActive && (
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

              {!codexActive && setup?.missing.includes('codex_integration') && (
                <Notice tone="warning" icon={AlertTriangle}>
                  {s.onboarding.codexMissing}
                </Notice>
              )}

              <div className="mt-6 flex items-center gap-3">
                {!codexActive && (
                  <Button
                    onClick={activateCodex}
                    disabled={phase.kind === 'working'}
                  >
                    {phase.kind === 'working' && <Loader2 className="mr-2 h-4 w-4 animate-spin motion-reduce:animate-none" />}
                    {phase.kind === 'working'
                      ? s.onboarding.codexActivating
                      : phase.kind === 'failed'
                        ? s.onboarding.retry
                        : status && !status.codex_cli_available && !codexActive
                          ? s.onboarding.retry
                        : s.onboarding.codexActivate}
                  </Button>
                )}
                <Button
                  variant={codexActive ? 'default' : 'ghost'}
                  onClick={() => void goTo('detect')}
                >
                  {codexActive ? s.onboarding.next : s.onboarding.skip}
                  <ArrowRight className="ml-2 h-4 w-4" />
                </Button>
              </div>

              <StepBack onClick={() => void goTo('welcome')} />
            </section>
          )}

          {step === 'detect' && (
            <section className="rounded-xl border border-border p-6">
              <h2 className="text-xl font-semibold tracking-tight">{s.onboarding.detectTitle}</h2>
              <p className="mt-2 text-sm text-muted-foreground">
                {s.onboarding.detectDescription}
              </p>

              {detection?.claude.detected && (
                <div className="mt-5 rounded-lg border border-border p-3">
                  <div className="flex items-center justify-between gap-3">
                    <div className="min-w-0">
                      <p className="font-medium">{s.onboarding.detectClaudeTitle}</p>
                      <p className="text-sm text-muted-foreground">
                        {detection.claude.already_imported
                          ? s.onboarding.detectImported
                          : detection.claude.logged_in === true
                            ? s.onboarding.detectClaudeLoggedIn
                            : detection.claude.logged_in === false
                              ? s.onboarding.detectClaudeNotLoggedIn
                              : s.onboarding.detectClaudeUnknown}
                      </p>
                    </div>
                    {!detection.claude.already_imported && detection.claude.logged_in === true && (
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => setConfirming('claude-code')}
                      >
                        {s.onboarding.detectClaudeImport}
                      </Button>
                    )}
                    {detection.claude.already_imported && (
                      <span className="shrink-0 text-xs font-medium text-emerald-600 dark:text-emerald-400">
                        {s.onboarding.detectImported}
                      </span>
                    )}
                  </div>
                  {detection.claude.logged_in === false && (
                    <p className="mt-2 text-xs text-muted-foreground">
                      {s.onboarding.detectClaudeComingLater}
                    </p>
                  )}
                </div>
              )}

              {detection && (
                <div className="mt-4 rounded-lg border border-border p-3">
                  <p className="font-medium">{s.onboarding.detectOpenCodeTitle}</p>
                  {detection.opencode.gateways.length === 0 ? (
                    <p className="mt-1 text-sm text-muted-foreground">
                      {s.onboarding.detectOpenCodeNone}
                    </p>
                  ) : (
                    <div className="mt-2 space-y-2">
                      {detection.opencode.gateways.map((gateway) => (
                        <div
                          key={gateway.id}
                          className="flex items-center justify-between gap-3 rounded-md bg-muted/40 p-2 text-sm"
                        >
                          <div className="min-w-0">
                            <p className="truncate font-medium" title={gateway.name}>{gateway.name}</p>
                            <p className="text-xs text-muted-foreground">
                              {gateway.already_imported
                                ? s.onboarding.detectImported
                                : gateway.importable
                                  ? s.onboarding.detectGatewayImportable
                                  : s.onboarding.detectGatewayNotImportable}
                            </p>
                          </div>
                          {!gateway.already_imported && gateway.importable && (
                            <Button
                              variant="outline"
                              size="sm"
                              onClick={() => setConfirming(gateway.id)}
                            >
                              {s.onboarding.detectImport}
                            </Button>
                          )}
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              )}

              {detection &&
                !detection.opencode.gateways.some((g) => g.importable && !g.already_imported) &&
                !(detection.claude.detected && detection.claude.logged_in === true) && (
                  <Notice tone="warning" icon={AlertTriangle}>
                    {s.onboarding.detectNothing}
                  </Notice>
                )}

              {detectError && (
                <Notice tone="danger" icon={AlertTriangle}>
                  {detectError}
                </Notice>
              )}

              {confirming && (
                <div className="mt-4 rounded-lg border border-border p-4">
                  <h3 className="font-semibold">
                    {s.onboarding.detectConfirmTitle.replace('{{name}}', gatewayName(confirming, s))}
                  </h3>
                  <p className="mt-1 text-sm text-muted-foreground">{s.onboarding.detectConfirmBody}</p>
                  <div className="mt-4 flex gap-2">
                    <Button onClick={confirmImport} disabled={importing}>
                      {importing && <Loader2 className="mr-2 h-4 w-4 animate-spin motion-reduce:animate-none" />}
                      {importing ? s.onboarding.detectImporting : s.onboarding.detectConfirm}
                    </Button>
                    <Button variant="ghost" onClick={() => setConfirming(null)} disabled={importing}>
                      {s.onboarding.detectCancel}
                    </Button>
                  </div>
                </div>
              )}

              <div className="mt-6 flex items-center gap-3">
                <Button variant="outline" onClick={() => void goTo('provider')}>
                  {s.onboarding.detectManual}
                  <ArrowRight className="ml-2 h-4 w-4" />
                </Button>
                <Button variant="ghost" onClick={() => void goTo('provider')}>
                  {s.onboarding.skip}
                  <ArrowRight className="ml-2 h-4 w-4" />
                </Button>
              </div>

              <StepBack onClick={() => void goTo('codex')} />
            </section>
          )}

          {step === 'provider' && (
            <section className="rounded-xl border border-border p-6">
              <h2 className="text-xl font-semibold tracking-tight">{s.onboarding.providerTitle}</h2>
              <p className="mt-2 text-sm text-muted-foreground">
                {s.onboarding.providerDescription}
              </p>

              {setup?.missing.includes('provider') && !savedProvider && (
                <Notice tone="warning" icon={AlertTriangle}>
                  {s.onboarding.providerNoSelection}
                </Notice>
              )}

              {savedProvider ? (
                <div className="mt-5">
                  <Notice tone="success" icon={Check}>
                    <span className="font-medium">{s.onboarding.providerSaved}</span>
                    <span className="block text-muted-foreground">
                      {s.onboarding.providerEnabledModels.replace(
                        '{{count}}',
                        String(savedProvider.models.filter((model) => model.enabled).length),
                      )}
                    </span>
                  </Notice>
                  <div className="mt-4 space-y-2">
                    {savedProvider.models.map((model) => (
                      <label
                        key={model.id}
                        className="flex min-w-0 items-center gap-3 rounded-md border border-border p-3 text-sm"
                      >
                        <Switch
                          checked={model.enabled}
                          onCheckedChange={(enabled) => toggleSavedModel(model.id, enabled)}
                          aria-label={model.label ?? model.id}
                        />
                        <span className="min-w-0 truncate" title={model.label ?? model.id}>{model.label ?? model.id}</span>
                      </label>
                    ))}
                    {savedProvider.models.length === 0 && (
                      <p className="text-sm text-muted-foreground">{s.onboarding.providerNoModels}</p>
                    )}
                  </div>
                </div>
              ) : (
                <div className="mt-5">
                  <div className="space-y-2">
                    <Select value={providerId || '__none'} onValueChange={selectProvider}>
                      <SelectTrigger aria-label={s.onboarding.providerChoose} className="w-full">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="__none">{s.onboarding.providerChoose}</SelectItem>
                        <SelectItem value="__recommend">{s.onboarding.providerRecommend}</SelectItem>
                        {WIZARD_PRESETS.map((preset) => (
                          <SelectItem key={preset.id} value={preset.id}>
                            {preset.name}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                    {selectedProvider && (
                      <p className="text-sm text-muted-foreground">
                        {s.onboarding[PRESET_HINTS[selectedProvider.id]]}
                      </p>
                    )}
                  </div>

                  {recommended && (
                    <Notice tone="success" icon={Sparkles}>
                      {s.onboarding.providerRecommendHint}
                    </Notice>
                  )}

                  {selectedProvider && (
                    <Button variant="link" size="sm" className="mt-2 px-0" onClick={openKeyLink}>
                      <ExternalLink className="h-3.5 w-3.5" />
                      {s.onboarding.providerKeyLink}
                    </Button>
                  )}

                  <Button
                    variant="ghost"
                    size="sm"
                    className="mt-3"
                    onClick={() => setCustom((value) => !value)}
                    aria-expanded={custom}
                  >
                    {s.onboarding.providerAdvanced}
                  </Button>
                  {custom && (
                    <p className="mb-2 text-xs text-muted-foreground">
                      {s.onboarding.providerAdvancedHint}
                    </p>
                  )}

                  <div className="mt-3 space-y-3">
                    {custom && (
                      <>
                        <Input
                          placeholder={s.onboarding.providerName}
                          value={customName}
                          onChange={(e) => setCustomName(e.target.value)}
                        />
                        <Input
                          placeholder={s.onboarding.providerBaseUrl}
                          value={customBaseUrl}
                          onChange={(e) => setCustomBaseUrl(e.target.value)}
                        />
                      </>
                    )}
                    {(!selectedProvider && !custom) ? null : (
                      <div>
                        <label htmlFor="onboarding-api-key" className="mb-1 block text-sm font-medium">
                          {s.onboarding.providerApiKey}
                        </label>
                        <Input
                          ref={keyInputRef}
                          id="onboarding-api-key"
                          type="password"
                          className="font-mono"
                          placeholder={s.onboarding.providerApiKey}
                          value={apiKey}
                          onChange={(e) => setApiKey(e.target.value)}
                          onFocus={() => setKeyFocused(true)}
                          onBlur={() => setKeyFocused(false)}
                        />
                        {keyFocused && (
                          <p className="mt-1 text-xs text-muted-foreground">
                            {s.onboarding.providerApiKeyHelp}
                          </p>
                        )}
                      </div>
                    )}
                    {providerError && (
                      <p role="alert" className="break-all text-sm text-destructive">
                        {providerError}
                      </p>
                    )}
                    {(!selectedProvider && !custom) ? null : (
                      <div className="flex flex-wrap gap-2">
                        <Button
                          onClick={() => saveProvider(false)}
                          disabled={validating || (!custom && !selectedProvider)}
                        >
                          {validating && <Loader2 className="mr-2 h-4 w-4 animate-spin motion-reduce:animate-none" />}
                          {validating
                            ? s.onboarding.providerValidating
                            : providerError
                              ? s.onboarding.providerRetry
                              : s.onboarding.providerValidate}
                        </Button>
                        {providerError && (
                          <Button variant="secondary" onClick={saveAnyway} disabled={validating}>
                            {s.onboarding.providerSaveAnyway}
                          </Button>
                        )}
                      </div>
                    )}
                  </div>
                </div>
              )}

              <div className="mt-6 flex items-center gap-3">
                <Button onClick={() => void goTo('validate')}>
                  {savedProvider ? s.onboarding.next : s.onboarding.skip}
                  <ArrowRight className="ml-2 h-4 w-4" />
                </Button>
              </div>
              <StepBack onClick={() => void goTo('detect')} />
            </section>
          )}

          {step === 'validate' && (
            <ValidationStep
              setup={setup}
              onBack={() => void goTo('provider')}
              onCheck={() => void reloadSetup()}
              onFinish={() => finish('/')}
              onLogs={() => finish('/logs')}
              onSkip={() => void goTo('agents')}
            />
          )}

          {step === 'agents' && (
            <AgentsStep
              multiAgent={multiAgent}
              busy={multiAgentBusy}
              error={multiAgentError}
              onToggle={toggleMultiAgent}
              onFinish={() => finish('/')}
              onBack={() => void goTo('provider')}
            />
          )}

          {step === 'finish' && (
            <FinishStep
              onFinish={() => finish('/')}
              onBack={() => void goTo('agents')}
            />
          )}
        </div>
      </div>
    </div>
  )
}

function gatewayName(id: GatewayId, s: Strings): string {
  if (id === 'claude-code') return s.onboarding.detectClaudeTitle
  return id === 'opencode-zen' ? 'OpenCode Zen' : 'OpenCode Go'
}
