import { useStrings } from '@/i18n'
import type { Locale } from '@/i18n'
import { Card, CardAction, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { modelLogoSrc, modelMonogram } from './modelLogo'

export interface ChartPoint {
  key: string
  label: string
  sublabel?: string
  cost: number
  latencyMs: number
  requests: number
}

interface AnalyticsChartProps {
  data: ChartPoint[]
  locale: Locale
  axisCost: string
  axisSpeed: string
  empty: string
  bubbleLegend: string
  title?: string
  subtitle?: string
  frontierLegend?: string
}

const WIDTH = 720
const HEIGHT = 460
const MARGIN = { top: 16, right: 16, bottom: 44, left: 56 }
const MIN_R = 5
const MAX_R = 18
// sqrt so bubble area scales linearly with request volume.
const RADIUS_K = (MAX_R - MIN_R) / Math.sqrt(10_000)
const LABEL_CHAR_WIDTH = 6.5
const LABEL_LINE_HEIGHT = 13
const LABEL_GAP = 18
const LABEL_OFFSET_Y = 4
const LABEL_NUDGE_STEP = 6

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value))
}

function fmtLatency(ms: number): string {
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${ms}ms`
}

function truncateModel(label: string): string {
  return label.length > 14 ? `${label.slice(0, 13)}...` : label
}

interface LabelBox {
  x: number
  y: number
  width: number
  height: number
}

function rectsOverlap(a: LabelBox, b: LabelBox): boolean {
  return (
    a.x < b.x + b.width &&
    a.x + a.width > b.x &&
    a.y < b.y + b.height &&
    a.y + a.height > b.y
  )
}

// Nudge label boxes vertically so they avoid each other and every bubble but
// their own, keeping each label as close to its natural spot as possible.
// eslint-disable-next-line react-refresh/only-export-components
export function resolveLabelOverlaps(
  labels: LabelBox[],
  bubbles: LabelBox[],
  minY: number,
  maxY: number,
  step: number,
): LabelBox[] {
  const placed: LabelBox[] = []
  return labels.map((label, index) => {
    const otherBubbles = bubbles.filter((_, other) => other !== index)
    const bottom = Math.max(minY, maxY - label.height)
    let resolved: LabelBox | null = null
    for (let distance = 0; distance <= 240; distance += step) {
      const offsets = distance === 0 ? [0] : [distance, -distance]
      for (const offset of offsets) {
        const candidate = { ...label, y: clamp(label.y + offset, minY, bottom) }
        if (
          placed.every((box) => !rectsOverlap(candidate, box)) &&
          otherBubbles.every((box) => !rectsOverlap(candidate, box))
        ) {
          resolved = candidate
          break
        }
      }
      if (resolved) break
    }
    const result = resolved ?? { ...label }
    placed.push(result)
    return result
  })
}

function niceStep(range: number): number {
  const rough = range / 4
  const magnitude = 10 ** Math.floor(Math.log10(rough))
  const normalized = rough / magnitude
  const step = normalized <= 1 ? 1 : normalized <= 2 ? 2 : normalized <= 5 ? 5 : 10
  return step * magnitude
}

// eslint-disable-next-line react-refresh/only-export-components
export function paretoFrontier(points: { cost: number; latency: number }[]): number[] {
  return points
    .map((_, index) => index)
    .filter((index) => {
      const point = points[index]
      return !points.some((other, otherIndex) => {
        if (index === otherIndex) return false
        return (
          other.cost <= point.cost &&
          other.latency <= point.latency &&
          (other.cost < point.cost || other.latency < point.latency)
        )
      })
    })
    .sort((a, b) => points[a].cost - points[b].cost || points[a].latency - points[b].latency)
}

export function AnalyticsChart({
  data,
  locale,
  axisCost,
  axisSpeed,
  empty,
  bubbleLegend,
  title = '',
  subtitle = '',
  frontierLegend = '',
}: AnalyticsChartProps) {
  const s = useStrings()

  const legend = (
    <div className="flex flex-col items-end gap-1">
      <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
        <span className="h-3 w-3 rounded-full bg-primary opacity-20" />
        {bubbleLegend}
      </span>
      <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
        <span className="w-5 border-t-2 border-dashed border-red-500" />
        {frontierLegend}
      </span>
    </div>
  )
  const header = (
    <CardHeader>
      <div className="min-w-0">
        <CardTitle className="text-base">{title}</CardTitle>
        {subtitle && <p className="mt-1 text-sm text-muted-foreground">{subtitle}</p>}
      </div>
      {frontierLegend ? <CardAction>{legend}</CardAction> : null}
    </CardHeader>
  )

  if (data.length === 0) {
    return (
      <Card>
        {header}
        <CardContent>
          <p className="text-sm text-muted-foreground">{empty}</p>
        </CardContent>
      </Card>
    )
  }

  const currency = new Intl.NumberFormat(locale, {
    style: 'currency',
    currency: 'USD',
    notation: 'compact',
  })
  const minCost = Math.min(...data.map((point) => point.cost))
  const maxCost = Math.max(...data.map((point) => point.cost))
  const minLatency = Math.min(...data.map((point) => point.latencyMs))
  const maxLatency = Math.max(...data.map((point) => point.latencyMs))
  const plotW = WIDTH - MARGIN.left - MARGIN.right
  const plotH = HEIGHT - MARGIN.top - MARGIN.bottom

  const xRange = Math.log10(maxCost) - Math.log10(minCost)
  const xPad = Math.max(xRange * 0.1, 0.1)
  const xMin = Math.log10(minCost) - xPad
  const xMax = Math.log10(maxCost) + xPad
  const x = (cost: number) =>
    MARGIN.left + ((Math.log10(cost) - xMin) / (xMax - xMin)) * plotW

  const yRange = maxLatency - minLatency
  const yPad = Math.max(yRange * 0.1, 1)
  const yMin = minLatency - yPad
  const yMax = maxLatency + yPad
  // Low latency (fast) at top, high latency (slow) at bottom, so "faster = up" matches the axis label.
  const y = (latencyMs: number) =>
    MARGIN.top + ((latencyMs - yMin) / (yMax - yMin)) * plotH

  const frontier = paretoFrontier(data.map((point) => ({ cost: point.cost, latency: point.latencyMs })))
  const radiusFor = (point: ChartPoint) =>
    clamp(MIN_R + Math.sqrt(point.requests) * RADIUS_K, MIN_R, MAX_R)
  const labelPositions = resolveLabelOverlaps(
    data.map((point) => {
      const label = truncateModel(point.label)
      const width = label.length * LABEL_CHAR_WIDTH
      return {
        x: clamp(x(point.cost) + LABEL_GAP, MARGIN.left, WIDTH - MARGIN.right - width),
        y: clamp(
          y(point.latencyMs) + LABEL_OFFSET_Y,
          MARGIN.top + 8,
          HEIGHT - MARGIN.bottom - LABEL_LINE_HEIGHT,
        ),
        width,
        height: LABEL_LINE_HEIGHT,
      }
    }),
    data.map((point) => {
      const r = radiusFor(point)
      const pad = 2
      return {
        x: x(point.cost) - r - pad,
        y: y(point.latencyMs) - r - pad,
        width: (r + pad) * 2,
        height: (r + pad) * 2,
      }
    }),
    MARGIN.top,
    HEIGHT - MARGIN.bottom,
    LABEL_NUDGE_STEP,
  )
  const xTicks: number[] = []
  for (let exp = Math.ceil(Math.log10(minCost)); exp <= Math.floor(Math.log10(maxCost)); exp += 1) {
    const value = 10 ** exp
    if (value >= minCost && value <= maxCost) xTicks.push(value)
  }

  const yStep = yRange > 0 ? niceStep(yRange) : 1
  const yTicks: number[] = []
  for (let value = Math.ceil(minLatency / yStep) * yStep; value <= maxLatency + yStep / 2; value += yStep) {
    yTicks.push(Math.round(value))
  }

  const xTickAnchor = (pos: number): 'start' | 'middle' | 'end' =>
    pos <= MARGIN.left + 20 ? 'start' : pos >= WIDTH - MARGIN.right - 20 ? 'end' : 'middle'

  return (
    <Card>
      {header}
      <CardContent>
        <svg
          viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
          className="h-auto w-full"
          preserveAspectRatio="xMidYMid meet"
        >
          <defs>
            <filter id="badge-shadow" x="-20%" y="-20%" width="140%" height="140%">
              <feDropShadow dx="0" dy="1" stdDeviation="1" floodColor="#000" floodOpacity="0.1" />
            </filter>
            {data.map((point) => {
              const clipId = `badge-${point.key.replace(/[^a-zA-Z0-9_-]/g, '-')}`
              return (
                <clipPath key={clipId} id={clipId}>
                  <circle cx={x(point.cost)} cy={y(point.latencyMs)} r={15} />
                </clipPath>
              )
            })}
          </defs>
          {xTicks.map((value) => {
            const pos = x(value)
            return (
              <line
                key={`grid-x-${value}`}
                x1={pos}
                y1={MARGIN.top}
                x2={pos}
                y2={HEIGHT - MARGIN.bottom}
                className="stroke-border/40"
              />
            )
          })}
          {yTicks.map((value) => {
            const pos = y(value)
            return (
              <line
                key={`grid-y-${value}`}
                x1={MARGIN.left}
                y1={pos}
                x2={WIDTH - MARGIN.right}
                y2={pos}
                className="stroke-border/40"
              />
            )
          })}
      <line
        x1={MARGIN.left}
        y1={MARGIN.top}
        x2={MARGIN.left}
        y2={HEIGHT - MARGIN.bottom}
        className="stroke-border"
      />
      <line
        x1={MARGIN.left}
        y1={HEIGHT - MARGIN.bottom}
        x2={WIDTH - MARGIN.right}
        y2={HEIGHT - MARGIN.bottom}
        className="stroke-border"
      />

      {xTicks.map((value) => {
        const pos = x(value)
        return (
          <g key={value}>
            <line
              x1={pos}
              y1={HEIGHT - MARGIN.bottom}
              x2={pos}
              y2={HEIGHT - MARGIN.bottom + 4}
              className="stroke-muted-foreground"
            />
            <text
              x={pos}
              y={HEIGHT - MARGIN.bottom + 16}
              textAnchor={xTickAnchor(pos)}
              className="fill-muted-foreground text-[10px]"
            >
              {currency.format(value)}
            </text>
          </g>
        )
      })}

      {yTicks.map((value) => {
        const pos = y(value)
        return (
          <g key={value}>
            <line
              x1={MARGIN.left - 4}
              y1={pos}
              x2={MARGIN.left}
              y2={pos}
              className="stroke-muted-foreground"
            />
            <text
              x={MARGIN.left - 6}
              y={pos + 3}
              textAnchor="end"
              className="fill-muted-foreground text-[10px]"
            >
              {fmtLatency(value)}
            </text>
          </g>
        )
      })}

      <text
        x={MARGIN.left + plotW / 2}
        y={HEIGHT - 12}
        textAnchor="middle"
        className="fill-foreground/70 text-xs"
      >
        {axisCost}
      </text>
      <text
        x={16}
        y={MARGIN.top + plotH / 2}
        textAnchor="middle"
        transform={`rotate(-90 16 ${MARGIN.top + plotH / 2})`}
        className="fill-foreground/70 text-xs"
      >
        {axisSpeed}
      </text>
      <text
        x={WIDTH - MARGIN.right}
        y={MARGIN.top + 12}
        textAnchor="end"
        className="fill-muted-foreground text-[10px]"
      >
        {bubbleLegend}
      </text>

      {frontier.length > 1 && (
        <polyline
          points={frontier
            .map((index) => `${x(data[index].cost)},${y(data[index].latencyMs)}`)
            .join(' ')}
          fill="none"
          stroke="#ef4444"
          strokeWidth={2}
          strokeDasharray="5 4"
          strokeLinecap="round"
        />
      )}

      {data.map((point, index) => {
        const cx = x(point.cost)
        const cy = y(point.latencyMs)
        const radius = radiusFor(point)
        const src = modelLogoSrc(point.key)
        const clipId = `badge-${point.key.replace(/[^a-zA-Z0-9_-]/g, '-')}`
        const label = truncateModel(point.label)
        const labelX = labelPositions[index].x
        const labelY = labelPositions[index].y
        const tooltip = [
          point.label,
          point.sublabel ?? '',
          currency.format(point.cost),
          fmtLatency(point.latencyMs),
          `${point.requests} ${s.overview.reqShort}`,
        ]
          .filter(Boolean)
          .join('\n')
        return (
          <g key={point.key} data-testid={`marker-${point.key}`}>
            <circle cx={cx} cy={cy} r={radius} className="fill-primary opacity-[0.14]">
              <title>{tooltip}</title>
            </circle>
            <g filter="url(#badge-shadow)">
              <circle cx={cx} cy={cy} r={16} className="fill-background stroke-border" />
              {src ? (
                <image
                  href={src}
                  x={cx - 15}
                  y={cy - 15}
                  width={30}
                  height={30}
                  preserveAspectRatio="xMidYMid slice"
                  clipPath={`url(#${clipId})`}
                />
              ) : (
                <text
                  x={cx}
                  y={cy}
                  textAnchor="middle"
                  dominantBaseline="central"
                  className="fill-muted-foreground text-[13px]"
                >
                  {modelMonogram(point.key)}
                </text>
              )}
            </g>
            <text
              x={labelX}
              y={labelY}
              textAnchor="start"
              className="fill-muted-foreground text-[10px]"
            >
              {label}
            </text>
          </g>
        )
      })}
        </svg>
      </CardContent>
    </Card>
  )
}
