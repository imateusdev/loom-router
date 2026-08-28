import { useEffect, useState } from 'react'
import { RefreshCw, X } from 'lucide-react'
import { isTauri } from '@/lib/api'
import { useStrings } from '@/i18n'
import { Button } from '@/components/ui/button'
import { Progress } from '@/components/ui/progress'
import type { Update } from '@tauri-apps/plugin-updater'

type Phase =
  | { kind: 'idle' }
  | { kind: 'checking' }
  | { kind: 'current' }
  | { kind: 'error' }
  | { kind: 'available'; version: string }
  | { kind: 'downloading'; percent: number }
  | { kind: 'ready' }

// Checks GitHub Releases for a newer version on app start and shows a
// banner to download, install and relaunch. Silent when offline, in the
// browser mock, or when already up to date.
export default function UpdateChecker() {
  const s = useStrings()
  const [phase, setPhase] = useState<Phase>({ kind: 'idle' })
  const [update, setUpdate] = useState<Update | null>(null)
  const [dismissed, setDismissed] = useState(false)

  const checkForUpdates = async (manual = false) => {
    if (!isTauri) return
    if (manual) {
      setDismissed(false)
      setPhase({ kind: 'checking' })
    }
    try {
      const { check } = await import('@tauri-apps/plugin-updater')
      const found = await check()
      if (found) {
        setUpdate(found)
        setPhase({ kind: 'available', version: found.version })
      } else if (manual) {
        setPhase({ kind: 'current' })
      }
    } catch {
      if (manual) setPhase({ kind: 'error' })
    }
  }

  useEffect(() => {
    if (!isTauri) return
    let cancelled = false
    import('@tauri-apps/plugin-updater')
      .then(({ check }) => check())
      .then((found) => {
        if (!cancelled && found) {
          setUpdate(found)
          setPhase({ kind: 'available', version: found.version })
        }
      })
      .catch(() => {
        // Automatic checks stay silent when offline or already current.
      })
    let unlisten: (() => void) | undefined
    // The chain above ends in a catch and this one did not, so a failed
    // dynamic import or a rejected `listen` left the tray's Check for Updates
    // item wired to nothing — no error anywhere, the menu entry just did not
    // respond. Reported now rather than dropped.
    import('@tauri-apps/api/event')
      .then(({ listen }) =>
        listen('loomrouter://check-updates', () => {
          if (!cancelled) void checkForUpdates(true)
        }).then((dispose) => {
          if (cancelled) dispose()
          else unlisten = dispose
        }),
      )
      .catch((e) => {
        console.error('tray update listener failed to register', e)
      })
    return () => {
      cancelled = true
      unlisten?.()
    }
  }, [])

  const install = async () => {
    if (!update) return
    setPhase({ kind: 'downloading', percent: 0 })
    let downloaded = 0
    let total = 0
    try {
      await update.downloadAndInstall((event) => {
        if (event.event === 'Started' && event.data.contentLength) {
          total = event.data.contentLength
        } else if (event.event === 'Progress') {
          downloaded += event.data.chunkLength
          if (total > 0) {
            setPhase({
              kind: 'downloading',
              percent: Math.min(100, Math.round((downloaded / total) * 100)),
            })
          }
        }
      })
      setPhase({ kind: 'ready' })
    } catch {
      // Download/install failed: hide the banner; the next app start
      // offers the update again.
      setPhase({ kind: 'idle' })
    }
  }

  const restart = async () => {
    const { relaunch } = await import('@tauri-apps/plugin-process')
    await relaunch()
  }

  if (phase.kind === 'idle' || dismissed) return null

  return (
    <div className="sticky top-0 z-10 flex items-center gap-3 border-b border-border bg-accent/90 px-4 py-2 text-sm backdrop-blur">
      <RefreshCw className="h-4 w-4 shrink-0" />
      {phase.kind === 'checking' && <span className="flex-1">{s.updater.checking}</span>}
      {phase.kind === 'current' && <span className="flex-1">{s.updater.current}</span>}
      {phase.kind === 'error' && <span className="flex-1">{s.updater.error}</span>}
      {phase.kind === 'available' && (
        <>
          <span className="flex-1">
            {s.updater.available.replace('{{version}}', phase.version)}
          </span>
          <Button size="sm" onClick={install}>
            {s.updater.install}
          </Button>
          <button
            onClick={() => setDismissed(true)}
            className="text-muted-foreground hover:text-foreground"
          >
            <X className="h-4 w-4" />
          </button>
        </>
      )}
      {phase.kind === 'downloading' && (
        <>
          <span>{s.updater.downloading}</span>
          <Progress value={phase.percent} className="w-40" />
          <span className="text-muted-foreground">{phase.percent}%</span>
        </>
      )}
      {phase.kind === 'ready' && (
        <>
          <span className="flex-1">{s.updater.ready}</span>
          <Button size="sm" onClick={restart}>
            {s.updater.restart}
          </Button>
        </>
      )}
      {(phase.kind === 'current' || phase.kind === 'error') && (
        <button
          onClick={() => setDismissed(true)}
          className="text-muted-foreground hover:text-foreground"
        >
          <X className="h-4 w-4" />
        </button>
      )}
    </div>
  )
}
