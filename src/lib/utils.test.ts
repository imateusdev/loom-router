import { describe, expect, it } from 'vitest'
import { formatContextWindow } from './utils'

describe('formatContextWindow', () => {
  it('keeps 1M-class windows readable', () => {
    // The raw divisor used to leak into the tag as "1.048576M".
    expect(formatContextWindow(1_048_576)).toBe('1.05M')
    expect(formatContextWindow(1_000_000)).toBe('1M')
    expect(formatContextWindow(1_050_000)).toBe('1.05M')
    expect(formatContextWindow(2_000_000)).toBe('2M')
  })

  it('labels sub-1M windows in the reading the vendor counted in', () => {
    // Binary: a decimal divisor would call these 131K / 262K / 205K.
    expect(formatContextWindow(131_072)).toBe('128K')
    expect(formatContextWindow(262_144)).toBe('256K')
    expect(formatContextWindow(204_800)).toBe('200K')
    // Decimal: a binary divisor would call these 488K / 391K / 195K.
    expect(formatContextWindow(500_000)).toBe('500K')
    expect(formatContextWindow(400_000)).toBe('400K')
    expect(formatContextWindow(200_000)).toBe('200K')
  })

  it('prefers the decimal reading when both divisors land whole', () => {
    // hy3 is advertised as 256k, not as the 250k its binary reading gives.
    expect(formatContextWindow(256_000)).toBe('256K')
    expect(formatContextWindow(128_000)).toBe('128K')
  })

  it('rounds windows that fit neither reading', () => {
    expect(formatContextWindow(262_140)).toBe('262K')
    expect(formatContextWindow(163_000)).toBe('163K')
  })
})
