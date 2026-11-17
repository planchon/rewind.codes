// Small SVG charts in the dashboard's tokens: runs by status over time, sparklines, meters. No
// chart library (archi/08-decisions.md): a few bars, each a link to the runs behind it, with a
// tooltip on hover and an accessible name. Colors are the run statuses', always with a legend.
import type { ReactNode } from "react";
import { Link } from "react-router";
import type { RunStatus, StatusCounts } from "../api/types";
import { formatCount } from "../model/format";
import { StatusGlyph } from "./Status";
import { useTooltip } from "./Tooltip";

/** The order of the stacked segments, from the baseline up. */
export const STACK_ORDER: readonly RunStatus[] = [
  "completed",
  "failed",
  "aborted",
  "cancelled",
  "active",
  "pending",
];

export interface StackedBucket {
  /** The bucket's start and end, in milliseconds since the epoch. */
  from: number;
  to: number;
  counts: StatusCounts;
}

export interface StackedBarsProps {
  buckets: StackedBucket[];
  /** What the bars count, for the accessible name: "Runs started". */
  title: string;
  /** How a bucket is named in its tooltip and label. */
  bucketLabel: (b: StackedBucket) => string;
  /** The runs behind a segment. */
  href: (b: StackedBucket, status: RunStatus) => string;
  /** Axis labels under the first, middle and last bars. */
  tick: (ms: number) => string;
  height?: number;
  /** The statuses' legend under the bars. */
  legend?: boolean;
}

const GAP = 2;

/** Runs by status over time: one bar per bucket, a segment per status, each a link. */
export function StackedBars({
  buckets,
  title,
  bucketLabel,
  href,
  tick,
  height = 120,
  legend = true,
}: StackedBarsProps) {
  const tip = useTooltip();
  const totals = buckets.map((b) => STACK_ORDER.reduce((n, s) => n + b.counts[s], 0));
  const max = Math.max(1, ...totals);
  const width = 600;
  const plot = height - 18;
  const slot = width / Math.max(1, buckets.length);
  const barWidth = Math.max(2, Math.min(28, slot - GAP * 2));
  const scale = (n: number) => (n / max) * (plot - 8);
  const present = STACK_ORDER.filter((s) => buckets.some((b) => b.counts[s] > 0));
  const ticks = [0, Math.floor((buckets.length - 1) / 2), buckets.length - 1].filter(
    (i, k, all) => i >= 0 && all.indexOf(i) === k,
  );

  return (
    <figure className="chart">
      {/* biome-ignore lint/a11y/useSemanticElements: SVG has no fieldset; its marks are links */}
      <svg
        viewBox={`0 0 ${width} ${height}`}
        preserveAspectRatio="none"
        className="chart-svg"
        role="group"
        aria-label={`${title}, by status, ${buckets.length} buckets; the tallest has ${formatCount(max)}`}
        style={{ height }}
      >
        <line className="chart-baseline" x1={0} x2={width} y1={plot + 0.5} y2={plot + 0.5} />
        {buckets.map((b, i) => {
          const x = i * slot + (slot - barWidth) / 2;
          let y = plot;
          const total = totals[i] ?? 0;
          return (
            <g key={b.from}>
              {STACK_ORDER.map((s) => {
                const n = b.counts[s];
                if (n === 0) return null;
                const h = Math.max(1.5, scale(n));
                y -= h;
                const top = y;
                y -= GAP;
                const label = `${bucketLabel(b)}: ${formatCount(n)} ${s} of ${formatCount(total)}`;
                const content = (
                  <>
                    <div className="tt-title">{bucketLabel(b)}</div>
                    <dl>
                      {STACK_ORDER.filter((x) => b.counts[x] > 0).map((x) => (
                        <ChartRow key={x} status={x} n={b.counts[x]} strong={x === s} />
                      ))}
                    </dl>
                  </>
                );
                return (
                  <Link
                    key={s}
                    to={href(b, s)}
                    aria-label={label}
                    onMouseMove={(e) => tip.show(e, content)}
                    onMouseLeave={tip.hide}
                    onFocus={tip.hide}
                  >
                    <rect
                      className={`chart-seg seg-${s}`}
                      x={x}
                      y={top}
                      width={barWidth}
                      height={h}
                      rx={1}
                    />
                  </Link>
                );
              })}
            </g>
          );
        })}
      </svg>
      <div className="chart-axis" aria-hidden="true">
        {ticks.map((i) => {
          const b = buckets[i];
          if (!b) return null;
          const style =
            i === 0
              ? { left: 0 }
              : i === buckets.length - 1
                ? { right: 0 }
                : { left: `${((i + 0.5) / buckets.length) * 100}%` };
          return (
            <span
              key={i}
              className={i === 0 || i === buckets.length - 1 ? "edge" : undefined}
              style={style}
            >
              {tick(b.from)}
            </span>
          );
        })}
      </div>
      {legend && present.length > 0 && (
        <figcaption className="chart-legend">
          {present.map((s) => (
            <span key={s} className={`status ${s}`}>
              <StatusGlyph status={s} size={11} />
              {s === "active" ? "still active" : s}
            </span>
          ))}
        </figcaption>
      )}
      {tip.view}
    </figure>
  );
}

function ChartRow({ status, n, strong }: { status: RunStatus; n: number; strong: boolean }) {
  return (
    <>
      <dt className={`status ${status}`}>
        <StatusGlyph status={status} size={11} />
        {status}
      </dt>
      <dd className="num" style={strong ? { fontWeight: 600 } : undefined}>
        {formatCount(n)}
      </dd>
    </>
  );
}

/** A count per bucket, as small columns: the shape of a failure group over the window. */
export function Sparkline({
  series,
  label,
  width = 96,
  height = 20,
}: {
  series: readonly number[];
  /** The accessible name: what the columns count. */
  label: string;
  width?: number;
  height?: number;
}) {
  if (series.length === 0) return <span className="muted">–</span>;
  const max = Math.max(1, ...series);
  const slot = width / series.length;
  const bar = Math.max(1, slot - (slot > 3 ? 1 : 0));
  const peak = Math.max(...series);
  return (
    <svg
      className="sparkline"
      viewBox={`0 0 ${width} ${height}`}
      width={width}
      height={height}
      role="img"
      aria-label={`${label}: ${formatCount(series.reduce((a, b) => a + b, 0))} in all, at most ${formatCount(peak)} in one bucket`}
    >
      <line className="chart-baseline" x1={0} x2={width} y1={height - 0.5} y2={height - 0.5} />
      {series.map((n, i) =>
        n === 0 ? null : (
          <rect
            // biome-ignore lint/suspicious/noArrayIndexKey: a bucket is its position
            key={i}
            className="spark-bar"
            x={i * slot}
            y={height - 1 - Math.max(1.5, (n / max) * (height - 2))}
            width={bar}
            height={Math.max(1.5, (n / max) * (height - 2))}
            rx={0.5}
          />
        ),
      )}
    </svg>
  );
}

/** A horizontal bar for a share of the largest value, beside a row's number. */
export function Meter({
  value,
  max,
  tone = "critical",
}: {
  value: number;
  max: number;
  tone?: "critical" | "warning" | "neutral" | "info";
}) {
  const share = max > 0 ? Math.min(1, value / max) : 0;
  return (
    <svg
      className={`meter tone-${tone}`}
      viewBox="0 0 100 6"
      preserveAspectRatio="none"
      aria-hidden="true"
    >
      <rect className="meter-track" x={0} y={0} width={100} height={6} rx={2} />
      {share > 0 && (
        <rect
          className="meter-bar"
          x={0}
          y={0}
          width={Math.max(2, share * 100)}
          height={6}
          rx={2}
        />
      )}
    </svg>
  );
}

/** A figure, as the Overview shows them: a big number, what it counts, a line of context; a link to what it counts, when there is one. */
export function Tile({
  to,
  label,
  value,
  context,
  tone,
}: {
  to?: string;
  label: string;
  value: ReactNode;
  context?: ReactNode;
  tone?: "critical" | "warning" | "good" | "info";
}) {
  const className = `tile${tone ? ` tile-${tone}` : ""}`;
  const body = (
    <>
      <span className="tile-label">{label}</span>
      <span className="tile-value num">{value}</span>
      {context !== undefined && <span className="tile-context">{context}</span>}
    </>
  );
  return to ? (
    <Link to={to} className={className}>
      {body}
    </Link>
  ) : (
    <div className={`${className} static`}>{body}</div>
  );
}
