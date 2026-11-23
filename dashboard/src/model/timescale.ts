// The time axis of the run timeline. A durable run can wait days between two tool calls that
// last milliseconds: on a linear axis those calls would vanish. Long idle gaps are compressed
// into fixed-width breaks, and the axis stays linear everywhere else.
import { formatDuration, formatOffset } from "./format";

export interface Break {
  from: number;
  to: number;
  /** Position of the break on the axis, as fractions of its width. */
  x0: number;
  x1: number;
}

export interface Tick {
  t: number;
  x: number;
  label: string;
  /** The first tick after the start or after a break: labelled with its full offset. */
  major: boolean;
}

export interface TimeScale {
  t0: number;
  t1: number;
  /** The position of a time on the axis, from 0 to 1. */
  x(t: number): number;
  breaks: Break[];
  ticks: Tick[];
}

export interface ScaleOptions {
  /** A gap shorter than this is never compressed: a slow tool call must look slow. */
  minBreakMs?: number;
  /** A gap is compressed when it is this many times longer than every shorter gap together. */
  ratio?: number;
  /** The width of one break, as a fraction of the axis. */
  breakWidth?: number;
  /** The least distance between two ticks, as a fraction of the axis. */
  tickSpacing?: number;
}

interface Segment {
  from: number;
  to: number;
  x0: number;
  x1: number;
  isBreak: boolean;
}

const STEPS = [
  1, 2, 5, 10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000, 15_000, 30_000, 60_000, 120_000,
  300_000, 600_000, 900_000, 1_800_000, 3_600_000, 7_200_000, 10_800_000, 21_600_000, 43_200_000,
  86_400_000, 172_800_000, 604_800_000, 1_209_600_000, 2_592_000_000,
];

function niceStep(raw: number): number {
  for (const step of STEPS) if (step >= raw) return step;
  return STEPS[STEPS.length - 1] ?? raw;
}

/** Builds the axis over the given times (entries, ends of open spans, now). */
export function buildTimeScale(times: readonly number[], options: ScaleOptions = {}): TimeScale {
  const minBreakMs = options.minBreakMs ?? 30_000;
  const ratio = options.ratio ?? 4;
  const tickSpacing = options.tickSpacing ?? 0.12;

  const points = [...new Set(times.filter(Number.isFinite))].sort((a, b) => a - b);
  const first = points[0] ?? 0;
  if (points.length < 2) points.splice(0, points.length, first, first + 1);
  const t0 = points[0] ?? first;
  const t1 = points[points.length - 1] ?? first + 1;

  const gaps = points
    .slice(1)
    .map((to, i) => ({ from: points[i] ?? to, to, d: to - (points[i] ?? to) }));

  // The shortest gap long enough to compress: every gap at least as long is compressed too.
  let threshold = Number.POSITIVE_INFINITY;
  let shorter = 0;
  for (const d of gaps.map((g) => g.d).sort((a, b) => a - b)) {
    if (shorter > 0 && d > Math.max(minBreakMs, ratio * shorter)) {
      threshold = d;
      break;
    }
    shorter += d;
  }
  const breakCount = gaps.filter((g) => g.d >= threshold).length;
  const linearTotal = gaps.filter((g) => g.d < threshold).reduce((sum, g) => sum + g.d, 0);
  const breakWidth = Math.min(options.breakWidth ?? 0.05, 0.4 / Math.max(breakCount, 1));
  const linearWidth = 1 - breakCount * breakWidth;

  const segments: Segment[] = [];
  let x = 0;
  for (const gap of gaps) {
    const isBreak = gap.d >= threshold;
    const width = isBreak ? breakWidth : linearTotal > 0 ? (gap.d / linearTotal) * linearWidth : 0;
    segments.push({ from: gap.from, to: gap.to, x0: x, x1: x + width, isBreak });
    x += width;
  }
  // Rounding must not leave the end of the axis short of 1.
  const last = segments[segments.length - 1];
  if (last) last.x1 = 1;

  const position = (t: number): number => {
    if (t <= t0) return 0;
    if (t >= t1) return 1;
    let lo = 0;
    let hi = segments.length - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      const segment = segments[mid];
      if (segment && segment.to < t) lo = mid + 1;
      else hi = mid;
    }
    const segment = segments[lo];
    if (!segment) return 0;
    const span = segment.to - segment.from;
    return span === 0
      ? segment.x0
      : segment.x0 + ((t - segment.from) / span) * (segment.x1 - segment.x0);
  };

  const breaks: Break[] = segments
    .filter((s) => s.isBreak)
    .map((s) => ({ from: s.from, to: s.to, x0: s.x0, x1: s.x1 }));

  // Sections are the linear stretches between breaks; each gets its own ticks. A break that ends
  // the axis, or follows another, has no stretch after it: its own label says how long it is.
  const ticks: Tick[] = [];
  let sectionStart = 0;
  for (let i = 0; i <= segments.length; i += 1) {
    const segment = segments[i];
    if (segment && !segment.isBreak) continue;
    if (i === sectionStart && i > 0) {
      sectionStart = i + 1;
      continue;
    }
    const from = segments[sectionStart]?.from ?? t0;
    const to = segments[i - 1]?.to ?? from;
    const xFrom = position(from);
    const xTo = segment ? segment.x0 : 1;
    // The start of a section wins over the minor ticks just before it; a section squeezed
    // between two breaks gets no label of its own, it would overlap the previous one.
    const close = (tick: Tick | undefined) =>
      tick !== undefined && xFrom - tick.x < tickSpacing * 0.5;
    while (close(ticks[ticks.length - 1]) && ticks[ticks.length - 1]?.major === false) ticks.pop();
    if (!close(ticks[ticks.length - 1])) {
      ticks.push({ t: from, x: xFrom, label: formatOffset(from - t0), major: true });
    }
    const room = Math.floor((xTo - xFrom) / tickSpacing);
    if (room >= 1 && to > from) {
      const step = niceStep((to - from) / room);
      for (let t = from + step; t <= to; t += step) {
        const tx = position(t);
        if (tx - xFrom < tickSpacing * 0.75 && ticks.length > 0) continue;
        // The label of the break that ends the section sits there.
        if (segment && xTo - tx < tickSpacing * 0.75) continue;
        ticks.push({ t, x: tx, label: `+${formatDuration(t - from)}`, major: false });
      }
    }
    sectionStart = i + 1;
  }

  return { t0, t1, x: position, breaks, ticks };
}
