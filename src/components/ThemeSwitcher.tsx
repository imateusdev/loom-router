// Compact theme toggle for the sidebar footer. Cycles system -> light ->
// dark; the icon alone says which one is active, so it needs no label beside
// it (the tooltip carries the translated name).

import { Monitor, Moon, Sun } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { useStrings } from '@/i18n'
import { setTheme, useTheme, type Theme } from '@/lib/theme'

const ORDER: Theme[] = ['system', 'light', 'dark']

const ICONS = { system: Monitor, light: Sun, dark: Moon } as const

export default function ThemeSwitcher() {
  const s = useStrings()
  const theme = useTheme()
  const Icon = ICONS[theme]
  const labels: Record<Theme, string> = {
    system: s.common.themeSystem,
    light: s.common.themeLight,
    dark: s.common.themeDark,
  }

  return (
    <Button
      variant="ghost"
      size="icon"
      className="h-8 w-8 shrink-0"
      title={labels[theme]}
      aria-label={labels[theme]}
      onClick={() => setTheme(ORDER[(ORDER.indexOf(theme) + 1) % ORDER.length])}
    >
      <Icon className="size-4" />
    </Button>
  )
}
