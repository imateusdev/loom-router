import { useEffect, useState } from 'react'
import { Routes, Route, useNavigate } from 'react-router'
import Layout from '@/components/Layout'
import OverviewPage from '@/pages/Overview'
import ProvidersPage from '@/pages/Providers'
import LogsPage from '@/pages/Logs'
import ServerPage from '@/pages/Server'
import CodexPage from '@/pages/Codex'
import AgentsPage from '@/pages/Agents'
import Onboarding from '@/pages/Onboarding'
import { api } from '@/lib/api'
import { useBackendState, useTrayNavigation } from '@/lib/events'
import { useStrings } from '@/i18n'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'

export default function App() {
  // Above the early returns below: hooks cannot be conditional, and the
  // tray must be able to open a page even while onboarding is undecided.
  const navigate = useNavigate()

  // null while undecided: rendering the app first and swapping to the
  // walkthrough a tick later would flash the wrong screen on every launch.
  const [needsOnboarding, setNeedsOnboarding] = useState<boolean | null>(null)

  // Routing a tray click into a router that is not on screen would look
  // like the menu did nothing; during the walkthrough the window is simply
  // brought to the front (the backend already did that) and nothing moves.
  useTrayNavigation((route) => {
    if (needsOnboarding === false) void navigate(route)
  })

  useEffect(() => {
    let cancelled = false
    api
      .getConfig()
      .then((config) => {
        if (!cancelled) setNeedsOnboarding(config.onboarding_completed !== true)
      })
      .catch(() => {
        // A config that cannot be read is not a reason to trap someone in
        // onboarding - fall through to the app.
        if (!cancelled) setNeedsOnboarding(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  if (needsOnboarding === null) return null
  if (needsOnboarding) return <Onboarding onDone={() => setNeedsOnboarding(false)} />

  return (
    <>
      <Routes>
        <Route element={<Layout />}>
          <Route path="/" element={<OverviewPage />} />
          <Route path="/providers" element={<ProvidersPage />} />
          <Route path="/logs" element={<LogsPage />} />
          <Route path="/server" element={<ServerPage />} />
          <Route path="/codex" element={<CodexPage />} />
          <Route path="/agents" element={<AgentsPage />} />
        </Route>
      </Routes>
      <CodexRepairModal />
    </>
  )
}

function CodexRepairModal() {
  const s = useStrings()
  const [orphaned, setOrphaned] = useState(false)
  const [dismissed, setDismissed] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const reload = () =>
    api.codexStatus().then((status) => {
      setOrphaned(status.managed_block_orphaned)
      if (!status.managed_block_orphaned) setDismissed(false)
    })
  useEffect(() => {
    void reload()
  }, [])
  // The tray can apply/remove the integration too; re-check on its events.
  useBackendState(reload)

  const repair = async () => {
    setBusy(true)
    setError(null)
    try {
      await api.codexApply()
      await reload()
    } catch (e) {
      setError(String(e instanceof Error ? e.message : e))
    } finally {
      setBusy(false)
    }
  }

  const ignore = () => setDismissed(true)

  return (
    <Dialog open={orphaned && !dismissed} onOpenChange={(open) => !open && ignore()}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{s.codex.repairTitle}</DialogTitle>
          <DialogDescription>{s.codex.orphanedHint}</DialogDescription>
        </DialogHeader>
        {error && <p className="text-xs text-red-600 dark:text-red-500">{error}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={ignore} disabled={busy}>
            {s.codex.ignore}
          </Button>
          <Button onClick={repair} disabled={busy}>
            {busy ? s.codex.repairing : s.codex.repair}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
