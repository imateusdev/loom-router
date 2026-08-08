import { useEffect, useRef, useState } from 'react'
import { NavLink, Outlet } from 'react-router'
import { Bot, Boxes, LayoutDashboard, Minus, Plus, ScrollText, Server, Sparkles } from 'lucide-react'
import { useStrings } from '@/i18n'
import LanguageSwitcher from '@/components/LanguageSwitcher'
import UpdateChecker from '@/components/UpdateChecker'
import { isTauri } from '@/lib/api'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'
import logo from '@/assets/logo.png'

const MIN_ZOOM = 100
const MAX_ZOOM = 200
const ZOOM_STEP = 10
const ZOOM_STORAGE_KEY = 'loomrouter.zoom'

function clampZoom(value: number): number {
  return Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, value))
}

export default function Layout() {
  const s = useStrings()
  const mainRef = useRef<HTMLElement | null>(null)
  const [zoom, setZoom] = useState(() => {
    try {
      const saved = Number(localStorage.getItem(ZOOM_STORAGE_KEY))
      return Number.isFinite(saved) ? clampZoom(saved) : 100
    } catch {
      return 100
    }
  })

  useEffect(() => {
    mainRef.current?.style.setProperty('zoom', `${zoom}%`)
  }, [zoom])

  useEffect(() => {
    try {
      localStorage.setItem(ZOOM_STORAGE_KEY, String(zoom))
    } catch {
      // Persistence is best-effort; zoom still works for the session.
    }
  }, [zoom])

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey)) return
      if (e.key === '+' || e.key === '=') {
        e.preventDefault()
        setZoom((z) => clampZoom(z + ZOOM_STEP))
      } else if (e.key === '-' || e.key === '_') {
        e.preventDefault()
        setZoom((z) => clampZoom(z - ZOOM_STEP))
      } else if (e.key === '0') {
        e.preventDefault()
        setZoom(100)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])
  // The binary's own version when running inside Tauri (this is what the
  // updater compares against), the package version in the browser mock.
  const [version, setVersion] = useState(__APP_VERSION__)
  useEffect(() => {
    if (!isTauri) return
    import('@tauri-apps/api/app')
      .then(({ getVersion }) => getVersion())
      .then(setVersion)
      .catch(() => {})
  }, [])
  const items = [
    { to: '/', icon: LayoutDashboard, label: s.nav.overview },
    { to: '/providers', icon: Boxes, label: s.nav.providers },
    { to: '/logs', icon: ScrollText, label: s.nav.logs },
    { to: '/server', icon: Server, label: s.nav.server },
    { to: '/codex', icon: Sparkles, label: s.nav.codex },
    { to: '/agents', icon: Bot, label: s.nav.agents },
  ]
  return (
    <div className="flex h-screen bg-background text-foreground">
      <aside className="w-60 shrink-0 border-r border-border flex flex-col">
        <div className="px-5 py-5">
          <div className="flex items-center gap-3">
            <img src={logo} alt="" className="h-9 w-9 rounded-lg" />
            <h1 className="text-lg font-semibold tracking-tight leading-tight">{s.app.name}</h1>
          </div>
          <p className="text-xs text-muted-foreground mt-2">{s.app.tagline}</p>
        </div>
        <nav className="flex-1 px-3 space-y-1">
          {items.map(({ to, icon: Icon, label }) => (
            <NavLink
              key={to}
              to={to}
              end={to === '/'}
              className={({ isActive }) =>
                cn(
                  'flex items-center gap-3 rounded-md px-3 py-2 text-sm transition-colors',
                  isActive
                    ? 'bg-accent text-accent-foreground font-medium'
                    : 'text-muted-foreground hover:bg-accent/50 hover:text-foreground',
                )
              }
            >
              <Icon className="h-4 w-4" />
              {label}
            </NavLink>
          ))}
        </nav>
        <div className="px-3 py-3 border-t border-border flex items-center gap-2">
          <div className="flex-1 min-w-0">
            <LanguageSwitcher />
          </div>
          <span className="shrink-0 text-xs text-muted-foreground">v{version}</span>
        </div>
      </aside>
      {/* `min-w-0`: a flex child defaults to min-width:auto and refuses to
          shrink below its content, so one wide element would push the pane
          past the window. `overflow-x-hidden` because `overflow-y-auto`
          makes the computed overflow-x `auto`, which would otherwise turn
          the whole pane into a horizontal scroller. */}
      <div className="fixed right-4 top-1.5 z-50 flex items-center gap-0.5 rounded-md border bg-background/95 p-0.5 shadow-sm">
        <Button
          variant="ghost"
          size="icon"
          title={s.common.zoomOut}
          aria-label={s.common.zoomOut}
          disabled={zoom <= MIN_ZOOM}
          className="h-6 w-6"
          onClick={() => setZoom((z) => clampZoom(z - ZOOM_STEP))}
        >
          <Minus className="h-3.5 w-3.5" />
        </Button>
        <button
          type="button"
          title={s.common.zoomReset}
          aria-label={s.common.zoomReset}
          onClick={() => setZoom(100)}
          className="min-w-[44px] rounded-md px-1.5 py-0.5 text-center text-[11px] font-medium tabular-nums hover:bg-accent"
        >
          {zoom}%
        </button>
        <Button
          variant="ghost"
          size="icon"
          title={s.common.zoomIn}
          aria-label={s.common.zoomIn}
          disabled={zoom >= MAX_ZOOM}
          className="h-6 w-6"
          onClick={() => setZoom((z) => clampZoom(z + ZOOM_STEP))}
        >
          <Plus className="h-3.5 w-3.5" />
        </Button>
      </div>
      <main ref={mainRef} className="min-w-0 flex-1 overflow-y-auto overflow-x-hidden">
        <UpdateChecker />
        <Outlet />
      </main>
    </div>
  )
}
