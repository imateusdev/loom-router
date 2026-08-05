// Compact language selector for the sidebar footer. Uses the native language
// names (plus an icon) so it needs no translatable label of its own.

import { Languages } from 'lucide-react'

import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { setLocale, useLocale, type Locale } from '@/i18n'

const OPTIONS: { value: Locale; label: string }[] = [
  { value: 'en', label: 'English' },
  { value: 'pt', label: 'Português (BR)' },
  { value: 'zh', label: '简体中文' },
  { value: 'es', label: 'Español' },
]

export default function LanguageSwitcher() {
  const locale = useLocale()

  return (
    <Select value={locale} onValueChange={(value) => setLocale(value as Locale)}>
      <SelectTrigger size="sm" className="w-full" aria-label="Language / Idioma / 语言">
        <Languages className="size-4" />
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {OPTIONS.map((option) => (
          <SelectItem key={option.value} value={option.value}>
            {option.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
}
