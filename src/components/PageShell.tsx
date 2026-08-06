// The frame every page sits in.
//
// This used to be six hand-rolled copies of `<div className="p-8 max-w-…">`,
// and they had drifted to two different widths — Overview and Logs at 6xl,
// the rest at 4xl — so pages visibly changed measure as you navigated. One
// component, one measure.

import type { ReactNode } from 'react'

/// Card layout for pages that show a set of self-contained panels.
///
/// `auto-fit` rather than a breakpoint on purpose: the content column is the
/// window minus the fixed 240px sidebar, so a viewport-width breakpoint would
/// always fire at the wrong moment. This reflows on the space the cards
/// actually have — side by side when two fit, stacked when they do not.
///
/// `items-start` keeps a short card from being stretched to match a tall
/// neighbour, which is what makes a grid of unequal cards look broken.
export const CARD_GRID =
  'grid items-start gap-6 grid-cols-[repeat(auto-fit,minmax(340px,1fr))]'

export default function PageShell({
  title,
  subtitle,
  actions,
  children,
}: {
  title: string
  subtitle?: ReactNode
  /// Filters, primary buttons — anything that belongs beside the title.
  actions?: ReactNode
  children: ReactNode
}) {
  return (
    // `mx-auto`: on a maximised window the content centres in the pane
    // instead of clinging to the sidebar with a void on the right.
    <div className="mx-auto w-full max-w-6xl px-6 py-8 min-[1264px]:px-8 min-[1800px]:max-w-[1600px]">
      <header className="mb-6 flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
        {/* `min-w-0` lets the subtitle wrap inside its own column rather than
            pushing the actions out of the row. */}
        <div className="min-w-0">
          <h2 className="text-2xl font-semibold tracking-tight">{title}</h2>
          {subtitle && <p className="mt-1 text-sm text-muted-foreground">{subtitle}</p>}
        </div>
        {actions && (
          // Wraps to its own line when the window is too narrow to hold both,
          // instead of crushing the controls.
          <div className="flex flex-wrap items-center gap-3">{actions}</div>
        )}
      </header>
      {children}
    </div>
  )
}
