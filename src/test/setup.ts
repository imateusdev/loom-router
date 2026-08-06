import '@testing-library/jest-dom/vitest'
import { cleanup } from '@testing-library/react'
import { afterEach, beforeEach } from 'vitest'

// Node >= 25 exposes an experimental global `localStorage` that shadows
// jsdom's and resolves to `undefined` unless `--localstorage-file` is passed.
// The setup clears it after every test, so restore a working Storage now to
// keep the suite green under both Node and Bun.
if (typeof window.localStorage === 'undefined') {
  const store = new Map<string, string>()
  Object.defineProperty(window, 'localStorage', {
    configurable: true,
    value: {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => {
        store.set(String(k), String(v))
      },
      removeItem: (k: string) => {
        store.delete(k)
      },
      clear: () => {
        store.clear()
      },
      key: (i: number) => [...store.keys()][i] ?? null,
      get length() {
        return store.size
      },
    },
  })
}

// jsdom implements neither of these, and Radix (Select, Dialog) calls both
// on mount. Without them every test that opens a menu or a modal throws.
beforeEach(() => {
  if (!Element.prototype.hasPointerCapture) {
    Element.prototype.hasPointerCapture = () => false
    Element.prototype.setPointerCapture = () => {}
    Element.prototype.releasePointerCapture = () => {}
  }
  if (!Element.prototype.scrollIntoView) {
    Element.prototype.scrollIntoView = () => {}
  }
  if (!window.matchMedia) {
    window.matchMedia = (query: string) =>
      ({
        matches: false,
        media: query,
        onchange: null,
        addEventListener: () => {},
        removeEventListener: () => {},
        addListener: () => {},
        removeListener: () => {},
        dispatchEvent: () => false,
      }) as unknown as MediaQueryList
  }
})

// Testing Library does not auto-clean when `globals` is on for every runner
// version; doing it here keeps each test's DOM its own.
afterEach(() => {
  cleanup()
  window.localStorage.clear()
})
