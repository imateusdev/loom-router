// Locales are deep partials merged over English, so a key added to en.ts and
// forgotten elsewhere must fall back rather than render `undefined`. Four
// locales times every new feature makes that a matter of when, not if.

import { describe, expect, it } from 'vitest'
import en from './en'
import pt from './pt'
import es from './es'
import zh from './zh'
import { stringsFor, type Locale } from './index'

const partials = { pt, es, zh } as const

/// Every leaf path in an object, as "a.b.c".
function paths(obj: unknown, prefix = ''): string[] {
  if (typeof obj !== 'object' || obj === null) return [prefix]
  return Object.entries(obj).flatMap(([k, v]) =>
    paths(v, prefix ? `${prefix}.${k}` : k),
  )
}

function at(obj: unknown, path: string): unknown {
  return path.split('.').reduce<unknown>((acc, k) => {
    if (typeof acc !== 'object' || acc === null) return undefined
    return (acc as Record<string, unknown>)[k]
  }, obj)
}

describe('locale files', () => {
  it('never invents a key English does not have', () => {
    // A typo in a locale file is otherwise invisible: it merges in as dead
    // weight and the real key silently falls back to English forever.
    for (const [name, partial] of Object.entries(partials)) {
      const extra = paths(partial).filter((p) => at(en, p) === undefined)
      expect(extra, `${name}.ts has keys absent from en.ts`).toEqual([])
    }
  })

  it('only ever holds strings as leaves', () => {
    for (const [name, partial] of Object.entries(partials)) {
      for (const p of paths(partial)) {
        expect(typeof at(partial, p), `${name}.${p}`).toBe('string')
      }
    }
  })
})

describe('runtime resolution', () => {
  it('resolves every English key in every locale', () => {
    // The guarantee the merge exists to provide: no screen can render
    // `undefined` because a translation is missing.
    for (const locale of ['en', 'pt', 'es', 'zh'] as Locale[]) {
      const s = stringsFor(locale)
      for (const p of paths(en)) {
        const value = at(s, p)
        expect(typeof value, `${locale}: ${p}`).toBe('string')
        expect((value as string).length, `${locale}: ${p} is empty`).toBeGreaterThan(0)
      }
    }
  })

  it('falls back to English for a key a locale does not translate', () => {
    // pt translates onboarding.start; if a key is missing anywhere the value
    // must still be the English one rather than undefined.
    const s = stringsFor('pt')
    expect(s.onboarding.start).toBe('Iniciar')
    // `app.name` is only defined in en.ts.
    expect(s.app.name).toBe(en.app.name)
  })
})
