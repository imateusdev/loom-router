import { NavLink, Outlet } from 'react-router'
import { Boxes, Server, Sparkles } from 'lucide-react'
import { useStrings } from '@/i18n'
import { cn } from '@/lib/utils'

export default function Layout() {
  const s = useStrings()
  const items = [
    { to: '/', icon: Boxes, label: s.nav.providers },
    { to: '/server', icon: Server, label: s.nav.server },
    { to: '/codex', icon: Sparkles, label: s.nav.codex },
  ]
  return (
    <div className="flex h-screen bg-background text-foreground">
      <aside className="w-60 shrink-0 border-r border-border flex flex-col">
        <div className="px-5 py-5">
          <h1 className="text-lg font-semibold tracking-tight">{s.app.name}</h1>
          <p className="text-xs text-muted-foreground mt-1">{s.app.tagline}</p>
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
      </aside>
      <main className="flex-1 overflow-y-auto">
        <Outlet />
      </main>
    </div>
  )
}
