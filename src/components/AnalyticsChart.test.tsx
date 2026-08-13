import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { AnalyticsChart, paretoFrontier, resolveLabelOverlaps, type ChartPoint } from './AnalyticsChart'

interface Box {
  x: number
  y: number
  width: number
  height: number
}

const overlaps = (a: Box, b: Box) =>
  a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y

describe('paretoFrontier', () => {
  it('returns only points not dominated on both cost and latency', () => {
    const points = [
      { cost: 10, latency: 100 },
      { cost: 5, latency: 50 },
      { cost: 7, latency: 40 },
      { cost: 4, latency: 80 },
    ]
    expect(paretoFrontier(points)).toEqual([3, 1, 2])
  })

  it('keeps every point when cost and latency trade off monotonically', () => {
    const points = [
      { cost: 1, latency: 100 },
      { cost: 2, latency: 80 },
      { cost: 3, latency: 60 },
    ]
    expect(paretoFrontier(points)).toEqual([0, 1, 2])
  })

  it('returns an empty array for no points', () => {
    expect(paretoFrontier([])).toEqual([])
  })
})

describe('resolveLabelOverlaps', () => {
  it('nudges a label away from an earlier label it would overlap', () => {
    const result = resolveLabelOverlaps(
      [
        { x: 10, y: 10, width: 20, height: 13 },
        { x: 12, y: 12, width: 20, height: 13 },
      ],
      [],
      0,
      100,
      6,
    )
    expect(overlaps(result[0], result[1])).toBe(false)
  })

  it('does not treat a label own bubble as an obstacle', () => {
    const box = { x: 10, y: 10, width: 20, height: 13 }
    const result = resolveLabelOverlaps([box], [box], 0, 100, 6)
    expect(result[0]).toEqual(box)
  })
})

describe('AnalyticsChart', () => {
  const baseProps = {
    locale: 'en' as const,
    axisCost: 'Avg cost / request (log)',
    axisSpeed: 'Speed (faster = up)',
    empty: 'No plottable usage yet.',
    bubbleLegend: 'Bubble size = requests',
    title: 'Cost vs speed by model',
    subtitle: 'Each bubble is a model; bigger means more requests.',
    frontierLegend: 'Pareto frontier',
  }

  it('renders one marker group per point with a logo image or monogram fallback', () => {
    const data: ChartPoint[] = [
      { key: 'opencode-go:deepseek-v4-flash', label: 'model-a', sublabel: 'Acme', cost: 1, latencyMs: 100, requests: 10 },
      { key: 'gpt-5.5', label: 'model-b', sublabel: 'Acme', cost: 2, latencyMs: 200, requests: 20 },
    ]
    const { container } = render(<AnalyticsChart data={data} {...baseProps} />)

    expect(screen.getByTestId('marker-opencode-go:deepseek-v4-flash')).toBeInTheDocument()
    expect(screen.getByTestId('marker-gpt-5.5')).toBeInTheDocument()
    expect(container.querySelectorAll('image')).toHaveLength(1)
    expect(screen.getByText('G')).toBeInTheDocument()
    expect(screen.getByText('model-a')).toBeInTheDocument()
  })

  it('renders the empty state without an SVG', () => {
    const { container } = render(<AnalyticsChart data={[]} {...baseProps} />)

    expect(screen.getByText('No plottable usage yet.')).toBeInTheDocument()
    expect(container.querySelector('svg')).toBeNull()
  })
})
