// Theme store. Mirrors the i18n module store (useSyncExternalStore, no
// Provider in the tree) so the switcher re-renders without threading state
// through Layout. Tailwind runs with darkMode: ["class"] and index.css
// already ships the `.dark` variable block, so applying a theme is only a
// matter of toggling `dark` on <html>.

import { useSyncExternalStore } from 'react'

export type Theme = 'system' | 'light' | 'dark'

const STORAGE_KEY = 'loomrouter.theme'

// jsdom (and restricted webviews) may not implement matchMedia.
const media =
  typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    ? window.matchMedia('(prefers-color-scheme: dark)')
    : null

function loadTheme(): Theme {
  try {
    const saved = window.localStorage.getItem(STORAGE_KEY)
    if (saved === 'system' || saved === 'light' || saved === 'dark') {
      return saved
    }
  } catch {
    // localStorage unavailable (e.g. restricted webview) - fall through.
  }
  return 'system'
}

let currentTheme: Theme = loadTheme()

const listeners = new Set<() => void>()

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

function emit(): void {
  for (const listener of listeners) listener()
}

/// The theme actually painted: `system` resolves against the OS preference.
export function resolveTheme(theme: Theme = currentTheme): 'light' | 'dark' {
  if (theme !== 'system') return theme
  return media?.matches ? 'dark' : 'light'
}

function apply(): void {
  document.documentElement.classList.toggle('dark', resolveTheme() === 'dark')
}

export function getTheme(): Theme {
  return currentTheme
}

export function setTheme(next: Theme): void {
  if (next === currentTheme) return
  currentTheme = next
  apply()
  try {
    window.localStorage.setItem(STORAGE_KEY, next)
  } catch {
    // Persistence is best-effort; the in-memory switch still applies.
  }
  emit()
}

export function useTheme(): Theme {
  return useSyncExternalStore(subscribe, getTheme)
}

// Run at import time: main.tsx pulls this in through Layout before the first
// render, so the initial paint already carries the right class - no flash of
// the light theme on startup.
apply()

// The OS can flip under us while `system` is selected.
media?.addEventListener('change', () => {
  if (currentTheme !== 'system') return
  apply()
  emit()
})
