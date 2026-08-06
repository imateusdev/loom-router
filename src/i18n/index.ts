// i18n entry point. English is the source locale; other locales are deep
// partials merged over English at runtime, so a missing key falls back to
// English. Locale state lives in a tiny module store (no Provider needed in
// the tree) and is consumed through useSyncExternalStore, so switching the
// language re-renders every component that calls useStrings().

import { useSyncExternalStore } from 'react'

import en, { type Strings } from './en'
import pt from './pt'
import zh from './zh'
import es from './es'

export type { Strings } from './en'

// Recursive partial that widens `as const` string literals to plain `string`
// so locale files can hold actual translations.
export type DeepPartial<T> = {
  [K in keyof T]?: T[K] extends string
    ? string
    : T[K] extends object
      ? DeepPartial<T[K]>
      : T[K]
}

export type Locale = 'en' | 'pt' | 'zh' | 'es'

const STORAGE_KEY = 'loomrouter.locale'

const partials: Record<Exclude<Locale, 'en'>, DeepPartial<Strings>> = {
  pt,
  zh,
  es,
}

// --- module store -----------------------------------------------------------

function loadLocale(): Locale {
  try {
    const saved = window.localStorage.getItem(STORAGE_KEY)
    if (saved === 'en' || saved === 'pt' || saved === 'zh' || saved === 'es') {
      return saved
    }
  } catch {
    // localStorage unavailable (e.g. restricted webview) — fall through.
  }
  return 'en'
}

let currentLocale: Locale = loadLocale()

const listeners = new Set<() => void>()

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

export function getLocale(): Locale {
  return currentLocale
}

export function setLocale(next: Locale): void {
  if (next === currentLocale) return
  currentLocale = next
  try {
    window.localStorage.setItem(STORAGE_KEY, next)
  } catch {
    // Persistence is best-effort; the in-memory switch still applies.
  }
  for (const listener of listeners) listener()
}

export function useLocale(): Locale {
  return useSyncExternalStore(subscribe, getLocale)
}

// --- deep merge + cache -----------------------------------------------------

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function deepMerge<T>(base: T, override: unknown): T {
  if (!isRecord(base) || !isRecord(override)) {
    return (override === undefined ? base : override) as T
  }
  const out: Record<string, unknown> = { ...base }
  for (const key of Object.keys(override)) {
    const baseValue = (base as Record<string, unknown>)[key]
    const overrideValue = override[key]
    out[key] =
      isRecord(baseValue) && isRecord(overrideValue)
        ? deepMerge(baseValue, overrideValue)
        : overrideValue === undefined
          ? baseValue
          : overrideValue
  }
  return out as T
}

// Merged snapshots are cached per locale so useSyncExternalStore / React sees
// a stable reference between locale changes.
const mergedCache = new Map<Locale, Strings>([['en', en]])

/// Resolved strings for a locale, outside React. `useStrings` is the hook
/// form; this is exported so the merge itself can be tested without
/// rendering a component.
export function stringsFor(locale: Locale): Strings {
  const cached = mergedCache.get(locale)
  if (cached) return cached
  const merged =
    locale === 'en' ? en : deepMerge(en, partials[locale])
  mergedCache.set(locale, merged)
  return merged
}

// --- public API (unchanged for components) ----------------------------------

export function useStrings(): Strings {
  return stringsFor(useLocale())
}

export function t(strings: Strings) {
  return strings
}
