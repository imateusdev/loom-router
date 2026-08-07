import { useEffect, useState } from 'react'
import { NavLink, Outlet } from 'react-router'
import { Bot, Boxes, LayoutDashboard, ScrollText, Server, Sparkles } from 'lucide-react'
import { useStrings } from '@/i18n'
import LanguageSwitcher from '@/components/LanguageSwitcher'
import UpdateChecker from '@/components/UpdateChecker'
import { isTauri } from '@/lib/api'
import { cn } from '@/lib/utils'
import logo from '@/assets/logo.png'

export default function Layout() {
  const s = useStrings()
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
      <main className="min-w-0 flex-1 overflow-y-auto overflow-x-hidden">
        <UpdateChecker />
        <Outlet />
      </main>
    </div>
  )
}
