// Backend → window events. The tray menu is a second author of the same
// state (server, active model, providers), so a page that only fetched on
// mount would keep showing what was true when it opened.

import { useEffect, useRef } from 'react'
import { isTauri } from '@/lib/api'

export const STATE_CHANGED = 'loomrouter://state-changed'
export const NAVIGATE = 'loomrouter://navigate'

// Subscribes to a backend event for the lifetime of the component.
//
// `listen()` resolves asynchronously, so the unsubscribe function can arrive
// after the effect was already cleaned up (React StrictMode mounts twice in
// dev) — the `disposed` flag is what stops that leaking a live listener.
function useTauriEvent<T>(event: string, handler: (payload: T) => void): void {
  // The handler is almost always a fresh closure per render (it reads state
  // setters); keeping it in a ref means the subscription survives renders
  // instead of being torn down and rebuilt on each one.
  const latest = useRef(handler)
  useEffect(() => {
    latest.current = handler
  })

  useEffect(() => {
    if (!isTauri) return
    let disposed = false
    let unlisten: (() => void) | undefined
    import('@tauri-apps/api/event')
      .then(({ listen }) => listen<T>(event, (e) => latest.current(e.payload)))
      .then((off) => {
        if (disposed) off()
        else unlisten = off
      })
      .catch(() => {
        // No event bridge (browser preview, or a denied capability): the
        // page simply keeps its fetch-on-mount behaviour.
      })
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [event])
}

/// Re-run `reload` whenever the backend changes state the page displays.
export function useBackendState(reload: () => void): void {
  useTauriEvent(STATE_CHANGED, reload)
}

/// Follow route changes requested from the tray menu.
export function useTrayNavigation(navigate: (route: string) => void): void {
  useTauriEvent<string>(NAVIGATE, navigate)
}
