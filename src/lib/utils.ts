import { clsx, type ClassValue } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

/**
 * Context window (tokens) as a compact tag: "1M", "1.05M", "256K", "500K".
 * Windows are published by the backend in raw tokens (e.g. kimi-k3 =
 * 1_048_576), so 1M-class models must not render as "1.048576M".
 */
export function formatContextWindow(window: number): string {
  if (window >= 1_000_000) {
    const m = window / 1_000_000
    return m >= 10 ? `${Math.round(m)}M` : `${m.toFixed(2).replace(/\.?0+$/, '')}M`
  }
  // Vendors publish sub-1M windows in both readings — grok-4.5 is a decimal
  // 500_000, kimi-k2.7-code a binary 262_144 — and neither divisor labels
  // both correctly: a fixed 1024 understates 500_000 as "488K", a fixed 1000
  // overstates 131_072 as "131K". Whichever divisor lands on a whole number
  // is the one the vendor counted in; decimal wins ties, since 256_000 is
  // advertised as 256k rather than the 250k its binary reading gives.
  const decimal = window / 1_000
  if (Number.isInteger(decimal)) return `${decimal}K`
  const binary = window / 1_024
  if (Number.isInteger(binary)) return `${binary}K`
  return `${Math.round(decimal)}K`
}
