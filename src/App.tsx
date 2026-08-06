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
import { useTrayNavigation } from '@/lib/events'

export default function App() {
  // Above the early returns below: hooks cannot be conditional, and the
  // tray must be able to open a page even while onboarding is undecided.
  const navigate = useNavigate()
  useTrayNavigation(navigate)

  // null while undecided: rendering the app first and swapping to the
  // walkthrough a tick later would flash the wrong screen on every launch.
  const [needsOnboarding, setNeedsOnboarding] = useState<boolean | null>(null)

  useEffect(() => {
    let cancelled = false
    api
      .getConfig()
      .then((config) => {
        if (!cancelled) setNeedsOnboarding(config.onboarding_completed !== true)
      })
      .catch(() => {
        // A config that cannot be read is not a reason to trap someone in
        // onboarding — fall through to the app.
        if (!cancelled) setNeedsOnboarding(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  if (needsOnboarding === null) return null
  if (needsOnboarding) return <Onboarding onDone={() => setNeedsOnboarding(false)} />

  return (
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
  )
}
