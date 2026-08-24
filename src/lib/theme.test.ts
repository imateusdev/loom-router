// The store is a module singleton applied to <html>, so the risk is not the
// cycle order but the two side effects: the class landing on the root element
// and the choice surviving a reload.

import { describe, expect, it } from 'vitest'
import { getTheme, resolveTheme, setTheme } from './theme'

describe('theme store', () => {
  it('toggles the dark class on <html> and persists the choice', () => {
    setTheme('dark')
    expect(document.documentElement.classList.contains('dark')).toBe(true)
    expect(localStorage.getItem('loomrouter.theme')).toBe('dark')

    setTheme('light')
    expect(document.documentElement.classList.contains('dark')).toBe(false)
    expect(localStorage.getItem('loomrouter.theme')).toBe('light')

    expect(getTheme()).toBe('light')
  })

  it('resolves system against the OS preference, not the stored value', () => {
    // jsdom reports no dark preference, so system must paint light even
    // though the previous test left the store on an explicit theme.
    setTheme('system')
    expect(resolveTheme()).toBe('light')
    expect(document.documentElement.classList.contains('dark')).toBe(false)
  })
})
