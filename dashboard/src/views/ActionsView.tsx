// Actions: latency, error rate and redeliveries per action, by group (an MCP server, an API, a
// group of harness actions), over a window, for the organization or one end user. Searched with
// the query bar of the runs, which reads `namespace:` and one `action:github.*` (sent to the server
// as the group), `action:` and an action's name here; its figures are the Overview's tiles, and
// each group of actions folds under its row.
import { ChevronDown, ChevronRight } from "lucide-react";
import { Fragment, useMemo, useState } from "react";
import { Link } from "react-router";
import { Empty, Loading, ProblemView, Time } from "../components/bits";
import { Tile } from "../components/Charts";
import { PageHeader } from "../components/PageHeader";
import { QueryBar } from "../components/QueryBar";
import { TableScroll } from "../components/TableScroll";
import { useTooltip } from "../components/Tooltip";
import {
  ACTIONS_LANGUAGE,
  actionsSearch,
  legacyActionsQuery,
  type RangeKey,
  toActionStatsQuery,
} from "../model/filters";
import { formatCount, formatDuration, formatPercent } from "../model/format";
import { type FieldValues, fieldTerm, queryLink, TEXT_VALUES } from "../model/query";
import type { ActionStats } from "../model/types";
import { useActionStats } from "../state/aggregates";
import { Seg } from "./FilterBits";
import { useListQuery, useWindow, WINDOW_WORDS, WINDOWS } from "./search";

const LEGACY_NAMES = ["toolset"] as const;

/** An action's group, the start of its name; harness actions and built-ins without one share one. */
export function groupOf(action: string): string {
  const at = action.indexOf(".");
  return at > 0 ? action.slice(0, at) : "harness & built-in";
}

interface Group {
  name: string;
  actions: ActionStats[];
  calls: number;
  errors: number;
  redeliveries: number;
}

export function groupActions(actions: ActionStats[]): Group[] {
  const groups = new Map<string, Group>();
  for (const a of actions) {
    const name = groupOf(a.action);
    let g = groups.get(name);
    if (!g) {
      g = { name, actions: [], calls: 0, errors: 0, redeliveries: 0 };
      groups.set(name, g);
    }
    g.actions.push(a);
    g.calls += a.calls;
    g.errors += a.errors;
    g.redeliveries += a.redeliveries;
  }
  return [...groups.values()].sort((a, b) => b.calls - a.calls || a.name.localeCompare(b.name));
}

const S = 1_000;
const M = 60 * S;
const H = 60 * M;
const D = 24 * H;

/**
 * The durations the latency axis is drawn at, as people name them: the major ones (1 ms, 10 ms,
 * 100 ms, 1 s, 10 s, 1 min, 10 min, 1 h, 1 d) label it, the others fill it when it is short.
 */
const STEPS: { ms: number; major: boolean }[] = [
  ...[1, 2, 5, 10, 20, 50, 100, 200, 500].map((ms) => ({ ms, major: [1, 10, 100].includes(ms) })),
  ...[1, 2, 5, 10, 20, 30].map((n) => ({ ms: n * S, major: n === 1 || n === 10 })),
  ...[1, 2, 5, 10, 20, 30].map((n) => ({ ms: n * M, major: n === 1 || n === 10 })),
  ...[1, 2, 6, 12].map((n) => ({ ms: n * H, major: n === 1 })),
  ...[1, 2, 7, 30].map((n) => ({ ms: n * D, major: n === 1 })),
];

/**
 * A log scale over the latencies of every action, so their strips compare: from the major step
 * under the fastest median to the step over the slowest call, so that both ends are ticks.
 */
export function logScale(actions: ActionStats[]) {
  // Calls answered at once (a test run, in virtual time) would stretch the axis to 1 ms.
  const positive = actions.map((t) => t.latencyMs.p50).filter((v) => v > 0);
  const fastest = Math.min(...positive, 1_000);
  const lo = STEPS.filter((s) => s.major && s.ms <= fastest).at(-1)?.ms ?? 1;
  const slowest = Math.max(...actions.map((t) => t.latencyMs.max), 10 * lo);
  const hi = STEPS.find((s) => s.ms >= slowest)?.ms ?? slowest;
  const span = Math.log10(hi / lo);
  return {
    x: (ms: number) => Math.min(1, Math.max(0, Math.log10(Math.max(ms, lo) / lo) / span)),
    min: lo,
    max: hi,
  };
}

/**
 * The ticks of the scale. Up to two major steps, every step is drawn and labelled; beyond, the
 * major steps only, and above five of them, every other one is labelled.
 */
export function logTicks(
  scale: ReturnType<typeof logScale>,
): { ms: number; major: boolean; label: boolean }[] {
  const inside = STEPS.filter((s) => s.ms >= scale.min && s.ms <= scale.max);
  const majors = inside.filter((s) => s.major);
  if (majors.length <= 2) return inside.map((s) => ({ ...s, label: true }));
  const every = majors.length > 5 ? 2 : 1;
  return majors.map((s, i) => ({ ...s, label: i % every === 0 }));
}

/** A tick's label: `10ms`, `1s`, `10s`, `1m`, `10m`, `1h`, `1d`. */
export function tickLabel(ms: number): string {
  if (ms < S) return `${ms}ms`;
  if (ms < M) return `${ms / S}s`;
  if (ms < H) return `${ms / M}m`;
  if (ms < D) return `${ms / H}h`;
  return `${ms / D}d`;
}

const pct = (x: number) => `${(x * 100).toFixed(2)}%`;

/** The axis of the latency column: a label at each tick, over the strips' gridlines. */
function LatencyAxis({ scale }: { scale: ReturnType<typeof logScale> }) {
  return (
    <div className="latency-axis" aria-hidden="true">
      {logTicks(scale)
        .filter((t) => t.label)
        .map((t) => (
          <span
            key={t.ms}
            className={t.major ? "major" : undefined}
            style={{ left: pct(scale.x(t.ms)) }}
          >
            {tickLabel(t.ms)}
          </span>
        ))}
    </div>
  );
}

/**
 * One action's latency on the shared scale: a dot at the median, a bar to p95, a line on to the
 * slowest call, a tick at p99; the gridlines of the axis behind.
 */
function LatencyStrip({
  stats,
  scale,
  onHover,
  onLeave,
}: {
  stats: ActionStats;
  scale: ReturnType<typeof logScale>;
  onHover: (e: React.MouseEvent) => void;
  onLeave: () => void;
}) {
  const { p50, p95, p99, max } = stats.latencyMs;
  const x = (ms: number) => pct(scale.x(ms));
  return (
    <div
      className="latency-strip"
      onMouseMove={onHover}
      onMouseLeave={onLeave}
      role="img"
      aria-label={`p50 ${formatDuration(p50)}, p95 ${formatDuration(p95)}, p99 ${formatDuration(p99)}, max ${formatDuration(max)}`}
    >
      {logTicks(scale).map((t) => (
        <span
          key={t.ms}
          className={`grid-line${t.major ? " major" : ""}`}
          style={{ left: x(t.ms) }}
        />
      ))}
      <span className="whisker" style={{ left: x(p95), width: `calc(${x(max)} - ${x(p95)})` }} />
      <span className="cap" style={{ left: x(max) }} />
      <span className="box" style={{ left: x(p50), width: `calc(${x(p95)} - ${x(p50)})` }} />
      <span className="tick p99" style={{ left: x(p99) }} />
      <span className="dot" style={{ left: x(p50) }} />
    </div>
  );
}

/** What the bar suggests: the actions of the window, and the groups they belong to. */
function actionValues(actions: ActionStats[]): FieldValues {
  const names = actions.map((t) => ({ value: t.action, count: t.calls, detail: "calls" }));
  const groups = [
    ...new Set(actions.filter((t) => t.action.includes(".")).map((t) => groupOf(t.action))),
  ].map((g) => ({ value: `${g}.*`, detail: "every action of the group" }));
  return { action: [...names, ...groups], [TEXT_VALUES]: names };
}

export function ActionsView() {
  const { q, setQ } = useListQuery(legacyActionsQuery, LEGACY_NAMES);
  const [win, setWin] = useWindow();
  const search = actionsSearch(q);
  const stats = useActionStats(
    `actions:${win}:${search.namespace ?? ""}:${search.group ?? ""}`,
    () => toActionStatsQuery(win, search, Date.now()),
  );
  const tip = useTooltip();
  const [folded, setFolded] = useState<ReadonlySet<string>>(new Set());
  const fold = (name: string) =>
    setFolded((current) => {
      const next = new Set(current);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });

  const all = stats.data?.actions ?? [];
  const values = useMemo(() => actionValues(stats.data?.actions ?? []), [stats.data]);
  const actions = all.filter((t) => search.keeps(t.action));
  const groups = groupActions(actions);
  const scale = logScale(actions);
  const calls = actions.reduce((s, t) => s + t.calls, 0);
  const errors = actions.reduce((s, t) => s + t.errors, 0);
  const redeliveries = actions.reduce((s, t) => s + t.redeliveries, 0);
  const slowest = [...actions].sort((a, b) => b.latencyMs.p95 - a.latencyMs.p95)[0];

  return (
    <div className="page">
      <PageHeader
        title="Actions"
        info={
          stats.data && (
            <p>
              Latency, error rate and redeliveries of the calls completed from{" "}
              <Time ms={Date.parse(stats.data.since)} /> to{" "}
              <Time ms={Date.parse(stats.data.until)} />.
            </p>
          )
        }
        actions={
          <Seg<RangeKey>
            label="Window"
            value={win}
            onChange={setWin}
            options={WINDOWS.map((w) => ({ value: w, label: w, title: WINDOW_WORDS[w] }))}
          />
        }
      />
      <QueryBar
        value={q}
        onSubmit={setQ}
        language={ACTIONS_LANGUAGE}
        values={values}
        fault={search.fault}
        placeholder="action:github.* namespace:alice close_issue"
      />

      {search.fault ? (
        <Empty>Fix the query to see the actions: the term at fault is underlined above.</Empty>
      ) : stats.isPending ? (
        <Loading>Loading action statistics…</Loading>
      ) : stats.isError ? (
        <ProblemView error={stats.error} title="The action statistics could not be read" />
      ) : actions.length === 0 ? (
        <div className="table-wrap">
          <Empty>
            {all.length === 0
              ? `No call completed in ${WINDOW_WORDS[win]}.`
              : "No action matches this search."}
          </Empty>
        </div>
      ) : (
        <>
          <section className="tiles" aria-label="Totals">
            <Tile
              label="Calls"
              value={formatCount(calls)}
              context={`${formatCount(actions.length)} action${actions.length === 1 ? "" : "s"} in ${WINDOW_WORDS[win]}`}
            />
            <Tile
              label="Failed"
              value={formatCount(errors)}
              context={`${formatPercent(errors, calls)} of the calls`}
              tone={errors > 0 ? "critical" : undefined}
            />
            <Tile
              label="Redeliveries"
              value={formatCount(redeliveries)}
              context={`${formatPercent(redeliveries, calls)} of the calls`}
              tone={redeliveries > 0 ? "warning" : undefined}
            />
            {slowest && (
              <Tile
                to={queryLink("/runs", fieldTerm("action", slowest.action))}
                label="Slowest p95"
                value={formatDuration(slowest.latencyMs.p95)}
                context={slowest.action}
              />
            )}
          </section>
          <TableScroll>
            <table className="grid actions-table">
              <thead>
                <tr>
                  <th>Action</th>
                  <th className="right">Calls</th>
                  <th>Failed</th>
                  <th>By kind</th>
                  <th className="right">Redeliveries</th>
                  <th className="right">p50</th>
                  <th className="right">p95</th>
                  <th className="right">p99</th>
                  <th className="right">max</th>
                  <th className="latency-head">
                    <span className="latency-title">
                      Latency <span className="muted">log scale</span>
                    </span>
                    <LatencyAxis scale={scale} />
                  </th>
                </tr>
              </thead>
              <tbody>
                {groups.map((g) => {
                  const open = !folded.has(g.name);
                  return (
                    <Fragment key={g.name}>
                      <tr className="group-row">
                        <td>
                          <button
                            type="button"
                            className="group-toggle mono"
                            aria-expanded={open}
                            onClick={() => fold(g.name)}
                          >
                            {open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                            {g.name}
                            <span className="count">{formatCount(g.actions.length)}</span>
                          </button>
                        </td>
                        <td className="right num">{formatCount(g.calls)}</td>
                        <td className="num">
                          {formatCount(g.errors)}{" "}
                          <span className="dim">· {formatPercent(g.errors, g.calls)}</span>
                        </td>
                        <td />
                        <td className="right num">{formatCount(g.redeliveries)}</td>
                        <td colSpan={5} />
                      </tr>
                      {open &&
                        g.actions.map((t) => (
                          <tr key={t.action}>
                            <td className="mono action-name">
                              <Link
                                to={queryLink("/runs", fieldTerm("action", t.action))}
                                title={`Runs that called ${t.action}`}
                              >
                                {t.action}
                              </Link>
                            </td>
                            <td className="right num">{formatCount(t.calls)}</td>
                            <td>
                              <span className="rate">
                                <span
                                  className={`rate-n num${t.errors > 0 ? " tone-critical" : " muted"}`}
                                >
                                  {formatCount(t.errors)}
                                </span>
                                <span className="rate-bar" aria-hidden="true">
                                  <span
                                    style={{
                                      width: `${t.calls ? (100 * t.errors) / t.calls : 0}%`,
                                    }}
                                  />
                                </span>
                                <span className="num dim">{formatPercent(t.errors, t.calls)}</span>
                              </span>
                            </td>
                            <td>
                              <span className="labels">
                                {Object.entries(t.errorKinds)
                                  .filter(([, n]) => n > 0)
                                  .sort(([, a], [, b]) => b - a)
                                  .map(([kind, n]) => (
                                    <Link
                                      key={kind}
                                      className="label"
                                      to={queryLink(
                                        "/runs",
                                        `${fieldTerm("action", t.action)} ${fieldTerm("error", kind)}`,
                                      )}
                                      title={`Runs where ${t.action} failed with ${kind}`}
                                    >
                                      <span className="k">{kind}</span>&nbsp;{n}
                                    </Link>
                                  ))}
                              </span>
                            </td>
                            <td
                              className="right num"
                              title={`${formatPercent(t.redeliveries, t.calls)} of calls`}
                            >
                              {formatCount(t.redeliveries)}
                            </td>
                            <td className="right num">{formatDuration(t.latencyMs.p50)}</td>
                            <td className="right num">{formatDuration(t.latencyMs.p95)}</td>
                            <td className="right num">{formatDuration(t.latencyMs.p99)}</td>
                            <td className="right num dim">{formatDuration(t.latencyMs.max)}</td>
                            <td className="latency-cell">
                              <LatencyStrip
                                stats={t}
                                scale={scale}
                                onLeave={tip.hide}
                                onHover={(e) =>
                                  tip.show(
                                    e,
                                    <>
                                      <div className="tt-title">{t.action}</div>
                                      <dl>
                                        <dt>p50</dt>
                                        <dd>{formatDuration(t.latencyMs.p50)}</dd>
                                        <dt>p95</dt>
                                        <dd>{formatDuration(t.latencyMs.p95)}</dd>
                                        <dt>p99</dt>
                                        <dd>{formatDuration(t.latencyMs.p99)}</dd>
                                        <dt>max</dt>
                                        <dd>{formatDuration(t.latencyMs.max)}</dd>
                                        <dt>calls</dt>
                                        <dd>{formatCount(t.calls)}</dd>
                                      </dl>
                                    </>,
                                  )
                                }
                              />
                            </td>
                          </tr>
                        ))}
                    </Fragment>
                  );
                })}
              </tbody>
            </table>
            <div className="list-foot latency-legend">
              <span className="legend-item">
                <span className="lg-dot" aria-hidden="true" /> p50
              </span>
              <span className="legend-item">
                <span className="lg-box" aria-hidden="true" /> p50 to p95
              </span>
              <span className="legend-item">
                <span className="lg-tick" aria-hidden="true" /> p99
              </span>
              <span className="legend-item">
                <span className="lg-whisker" aria-hidden="true" /> to the slowest call
              </span>
              <span className="muted">
                On a log scale. Latency runs from the call's schedule to its completion.
              </span>
            </div>
          </TableScroll>
        </>
      )}
      {tip.view}
    </div>
  );
}
