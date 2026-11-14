// A namespace's CPU time added up over a month, against its quota: a small SVG in the dashboard's
// tokens, as the other charts (no chart library). The quota is a dashed line; where the use
// crossed 80 % and 100 % of it is marked; the line keeps rising above the quota, since CPU time is
// never refused. Each day shows its figures on hover.
import type { MouseEvent } from "react";
import {
  type CumulativePoint,
  crossingDay,
  dayLabel,
  formatCpu,
  formatShare,
  levelOf,
  monthLabel,
  shareOf,
} from "../model/quotas";
import { useTooltip } from "./Tooltip";

const WIDTH = 640;
const HEIGHT = 220;
const LEFT = 46;
const RIGHT = 92;
const TOP = 16;
const BOTTOM = 26;
const PLOT_W = WIDTH - LEFT - RIGHT;
const PLOT_H = HEIGHT - TOP - BOTTOM;

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
/** Steps of the vertical axis, the first that gives at most five lines. */
const STEPS = [
  MINUTE,
  5 * MINUTE,
  10 * MINUTE,
  15 * MINUTE,
  30 * MINUTE,
  HOUR,
  2 * HOUR,
  5 * HOUR,
  10 * HOUR,
  20 * HOUR,
  50 * HOUR,
  100 * HOUR,
  200 * HOUR,
  500 * HOUR,
  1_000 * HOUR,
];

export interface CpuChartProps {
  /** One point per day, from the 1st (see `cumulativeCpu`). */
  points: readonly CumulativePoint[];
  month: string;
  days: number;
  /** The quota, in milliseconds; `null` when CPU time is not limited. */
  limit: number | null;
  /** Whose use it is, for the accessible name. */
  name: string;
}

function step(max: number): number {
  return STEPS.find((s) => max / s <= 5) ?? STEPS[STEPS.length - 1] ?? HOUR;
}

export function CpuChart({ points, month, days, limit, name }: CpuChartProps) {
  const tip = useTooltip();
  const last = points[points.length - 1] ?? null;
  const top = Math.max(limit ?? 0, last?.ms ?? 0, MINUTE);
  const unit = step(top * 1.1);
  const yMax = Math.ceil((top * 1.1) / unit) * unit;
  const x = (day: number) => LEFT + (day / days) * PLOT_W;
  const y = (ms: number) => TOP + PLOT_H - (ms / yMax) * PLOT_H;
  const ticks = Array.from({ length: Math.round(yMax / unit) + 1 }, (_, i) => i * unit);
  const line = [`M${x(0)} ${y(0)}`, ...points.map((p) => `L${x(p.day)} ${y(p.ms)}`)].join(" ");
  const area = last ? `${line} L${x(last.day)} ${y(0)} Z` : "";
  const warning = limit ? crossingDay(points, limit, 80) : null;
  const critical = limit ? crossingDay(points, limit, 100) : null;
  const level = last && limit !== null ? levelOf(shareOf(last.ms, limit)) : "none";
  const dayTicks = [1, 8, 15, 22, 29].filter((d) => d <= days);

  const summary = [
    `Cumulative CPU time of ${name} in ${monthLabel(month)}`,
    last ? `${formatCpu(last.ms)} by ${dayLabel(month, last.day)}` : "no use",
    limit !== null ? `against a quota of ${formatCpu(limit)}` : "without a quota",
    warning ? `80% reached on ${dayLabel(month, warning.day)}` : null,
    critical ? `100% on ${dayLabel(month, critical.day)}` : null,
  ]
    .filter(Boolean)
    .join(", ");

  /** The day under the pointer, from its position in the drawing. */
  const onMove = (e: MouseEvent<SVGSVGElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    if (box.width === 0) return;
    const at = ((e.clientX - box.left) / box.width) * WIDTH;
    const index = Math.ceil(((at - LEFT) / PLOT_W) * days) - 1;
    const p = points[index];
    if (!p || at < LEFT) {
      tip.hide();
      return;
    }
    hover(e, p, points[index - 1]?.ms ?? 0);
  };

  const hover = (e: MouseEvent, p: CumulativePoint, previous: number) =>
    tip.show(
      e,
      <>
        <div className="tt-title">{dayLabel(month, p.day)}</div>
        <dl>
          <dt>That day</dt>
          <dd className="num">{formatCpu(p.ms - previous)}</dd>
          <dt>Since the 1st</dt>
          <dd className="num">{formatCpu(p.ms)}</dd>
          {limit !== null && (
            <>
              <dt>Of the quota</dt>
              <dd className="num">{formatShare(shareOf(p.ms, limit) ?? 0)}</dd>
            </>
          )}
        </dl>
      </>,
    );

  return (
    <figure className="chart cpu-chart">
      <svg
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        className="cpu-svg"
        role="img"
        aria-label={`${summary}.`}
        onMouseMove={onMove}
        onMouseLeave={tip.hide}
      >
        {ticks.map((t) => (
          <g key={t}>
            <line className="cpu-grid" x1={LEFT} x2={LEFT + PLOT_W} y1={y(t)} y2={y(t)} />
            <text className="cpu-axis" x={LEFT - 8} y={y(t) + 3.5} textAnchor="end">
              {formatCpu(t)}
            </text>
          </g>
        ))}
        {dayTicks.map((d) => (
          <text key={d} className="cpu-axis" x={x(d - 1)} y={TOP + PLOT_H + 16} textAnchor="middle">
            {d}
          </text>
        ))}
        {limit !== null && (
          <g>
            <line className="cpu-quota" x1={LEFT} x2={LEFT + PLOT_W} y1={y(limit)} y2={y(limit)} />
            <text className="cpu-quota-label" x={LEFT + PLOT_W} y={y(limit) - 6} textAnchor="end">
              quota {formatCpu(limit)}
            </text>
          </g>
        )}
        {last && (
          <>
            <path className={`cpu-area level-${level}`} d={area} />
            <path className={`cpu-line level-${level}`} d={line} />
            <line
              className="cpu-today"
              x1={x(last.day)}
              x2={x(last.day)}
              y1={TOP}
              y2={TOP + PLOT_H}
            />
            <circle className={`cpu-dot level-${level}`} cx={x(last.day)} cy={y(last.ms)} r={4} />
            <text className="cpu-label" x={x(last.day) + 8} y={y(last.ms) - 2}>
              {formatCpu(last.ms)}
            </text>
            <text className="cpu-label-sub" x={x(last.day) + 8} y={y(last.ms) + 12}>
              by {dayLabel(month, last.day)}
            </text>
          </>
        )}
        {warning && (
          <g>
            <circle
              className="cpu-mark level-warning"
              cx={x(warning.day)}
              cy={y(warning.ms)}
              r={5}
            />
            <text
              className="cpu-mark-label level-warning"
              x={x(warning.day) + 8}
              y={y(warning.ms) + 16}
            >
              80% · {dayLabel(month, warning.day)}
            </text>
          </g>
        )}
        {critical && (
          <g>
            <circle
              className="cpu-mark level-critical"
              cx={x(critical.day)}
              cy={y(critical.ms)}
              r={5}
            />
            <text
              className="cpu-mark-label level-critical"
              x={x(critical.day) - 8}
              y={y(critical.ms) - 10}
              textAnchor="end"
            >
              100% · {dayLabel(month, critical.day)}
            </text>
          </g>
        )}
      </svg>
      {tip.view}
    </figure>
  );
}
