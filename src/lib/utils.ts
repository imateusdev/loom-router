import { clsx, type ClassValue } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

/**
 * Context window (tokens) as a compact tag: "1M", "1.05M", "256K".
 * Windows are published by the backend in raw tokens (e.g. kimi-k3 =
 * 1_048_576), so 1M-class models must not render as "1.048576M".
 */
export function formatContextWindow(window: number): string {
  if (window >= 1_000_000) {
    const m = window / 1_000_000
    return m >= 10 ? `${Math.round(m)}M` : `${m.toFixed(2).replace(/\.?0+$/, '')}M`
  }
  return `${Math.round(window / 1024)}K`
}
