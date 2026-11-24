// Overview, the home page: the runs of a window by outcome over time, the share that completed,
// durations, the runs waiting now, quarantined runs, what fails most and the failing actions. Every
// figure, bar and row opens the runs behind it. Its figures are the statistics of the query's
// namespace, or of the organization; the query's other terms narrow the runs its links open. Live:
// its figures change in place within 10 s of a run's change.
import type { ReactNode } from "react";
import { Link } from "react-router";
import { useSettings } from "../app/settings";
import { Empty, Loading, ProblemView } from "../components/bits";
import { Meter, StackedBars, Tile } from "../components/Charts";
import { PageHeader } from "../components/PageHeader";
import { QueryBar } from "../components/QueryBar";
import { isCancellation } from "../model/ending";
import { RANGES, type RangeKey } from "../model/filters";
import { formatCount, formatDuration, formatPercent, formatTime } from "../model/format";
import { fieldTerm, joinQuery, queryLink, queryTime } from "../model/query";
import { statsScope, waitingRuns } from "../model/scope";
import type { RunStatus } from "../model/types";
import { useActionStats, useFailures, useRunStats } from "../state/aggregates";
import { queryFault } from "../state/errors";
import { Seg } from "./FilterBits";
import {
  INTERVALS,
  useRunQueryValues,
  useSearchQuery,
  useWindow,
  WINDOW_WORDS,
  WINDOWS,
  windowTerm,
} from "./search";

/** The change from the previous window, as people say it. */
function change(now: number, before: number, win: RangeKey): string {
  const previous = `the previous ${win}`;
  if (before === 0) return now === 0 ? `none in ${previous} either` : `none in ${previous}`;
  const delta = Math.round((100 * (now - before)) / before);
  if (delta === 0) return `as many as in ${previous}`;
  return `${delta > 0 ? "+" : ""}${delta}% on ${previous}`;
}

/** An axis label for an interval's start: a time of day within a day, a date beyond. */
function tickFormat(win: RangeKey, zone: "utc" | "local") {
  return (ms: number) => {
    const t = formatTime(ms, zone);
    if (win === "1h" || win === "24h") return t.slice(11, 16);
    if (win === "7d") return `${t.slice(5, 10)} ${t.slice(11, 13)}h`;
    return t.slice(5, 10);
  };
}

export function OverviewView() {
  const { q, setQ } = useSearchQuery();
  const [win, setWin] = useWindow();
  const { zone } = useSettings();
  const lookup = useRunQueryValues();
  const width = RANGES[win];
  const { interval, ms: intervalMs } = INTERVALS[win];
  const key = `${win}|${q}`;
  const withQ = q ? { q } : {};

  const scope = statsScope(q);
  const current = useRunStats(`overview|${key}`, () => {
    const now = Date.now();
    return {
      ...withQ,
      since: new Date(now - width).toISOString(),
      until: new Date(now).toISOString(),
      interval,
      top: 10,
    };
  });
  const previous = useRunStats(`overview-previous|${key}`, () => {
    const now = Date.now();
    return {
      ...withQ,
      since: new Date(now - 2 * width).toISOString(),
      until: new Date(now - width).toISOString(),
      top: 1,
    };
  });
  const failures = useFailures(`overview-failures|${key}`, () => {
    const now = Date.now();
    return {
      ...withQ,
      since: new Date(now - width).toISOString(),
      until: new Date(now).toISOString(),
      limit: 5,
    };
  });
  const actions = useActionStats(`overview-actions|${win}|${scope.q ?? ""}`, () => {
    const now = Date.now();
    return {
      ...(scope.q ? { q: scope.q } : {}),
      since: new Date(now - width).toISOString(),
      until: new Date(now).toISOString(),
    };
  });

  const fault = queryFault(current.error);
  const started = windowTerm("started", win);
  const runsLink = (...terms: string[]) => queryLink("/runs", joinQuery(q, started, ...terms));

  let body: ReactNode;
  if (fault) {
    body = <Empty>Fix the query to see the overview: the term at fault is underlined above.</Empty>;
  } else if (current.isPending) {
    body = <Loading>Loading the overview…</Loading>;
  } else if (current.isError) {
    body = <ProblemView error={current.error} title="The statistics could not be read" />;
  } else {
    const s = current.data;
    const ended =
      s.statuses.completed + s.statuses.failed + s.statuses.cancelled + s.statuses.aborted;
    const failed = s.statuses.failed + s.statuses.aborted + s.statuses.cancelled;
    const durations = s.durations;
    const since = s.since ? Date.parse(s.since) : Date.now() - width;
    const buckets = (s.series ?? []).map((b) => {
      const from = Date.parse(b.at);
      return { from, to: from + intervalMs, counts: b.statuses };
    });
    const tick = tickFormat(win, zone);
    const failingActions = (actions.data?.actions ?? [])
      .filter((t) => t.errors > 0)
      .map((t) => {
        const [kind = "error", n = t.errors] =
          Object.entries(t.errorKinds).sort(([, a], [, b]) => b - a)[0] ?? [];
        return { action: t.action, kind, n, errors: t.errors, calls: t.calls };
      })
      .sort((a, b) => b.errors - a.errors || a.action.localeCompare(b.action))
      .slice(0, 5);
    const fingerprints = failures.data?.fingerprints ?? [];
    body = (
      <>
        <section className="tiles" aria-label="Figures">
          <Tile
            to={runsLink()}
            label="Runs started"
            value={formatCount(s.runs)}
            context={previous.data ? change(s.runs, previous.data.runs, win) : " "}
          />
          <Tile
            to={runsLink("status:completed")}
            label="Completed"
            value={formatPercent(s.statuses.completed, ended)}
            context={`of the ${formatCount(ended)} runs that ended`}
            tone="good"
          />
          <Tile
            to={queryLink("/runs/failures", q, win === "24h" ? {} : { window: win })}
            label="Failed"
            value={formatCount(s.statuses.failed)}
            context={`+ ${formatCount(s.statuses.aborted)} aborted, ${formatCount(s.statuses.cancelled)} cancelled`}
            tone={failed > 0 ? "critical" : undefined}
          />
          <Tile
            to={queryLink("/runs/waiting", q)}
            label="Waiting now"
            value={formatCount(waitingRuns(s.waiting))}
            context={`${formatCount(s.waiting.signal)} on a signal`}
            tone="info"
          />
          <Tile
            to={durations ? runsLink(`duration:>=${durations.p95}`) : runsLink()}
            label="Duration p95"
            value={durations ? formatDuration(durations.p95) : "–"}
            context={durations ? `p50 ${formatDuration(durations.p50)}` : "no run ended"}
          />
          <Tile
            to={queryLink("/runs", joinQuery(q, "waiting:quarantined"))}
            label="Quarantined"
            value={formatCount(s.quarantined)}
            context="held until an operator acts"
            tone={s.quarantined > 0 ? "warning" : undefined}
          />
        </section>

        <section className="panel" aria-label="Runs by outcome">
          <header className="panel-head">
            <h2>Runs started, by outcome</h2>
            <span className="muted">
              {WINDOW_WORDS[win]}, by {formatDuration(intervalMs)}
            </span>
          </header>
          <div className="panel-body">
            {buckets.length === 0 || s.runs === 0 ? (
              <Empty>No run started in {WINDOW_WORDS[win]}.</Empty>
            ) : (
              <StackedBars
                buckets={buckets}
                title="Runs started"
                tick={tick}
                bucketLabel={(b) => `${formatTime(b.from, zone).slice(0, 16)} – ${tick(b.to)}`}
                href={(b, status: RunStatus) =>
                  queryLink(
                    "/runs",
                    joinQuery(
                      q,
                      `status:${status}`,
                      `started:${queryTime(Math.max(b.from, since))}..${queryTime(b.to)}`,
                    ),
                  )
                }
              />
            )}
          </div>
        </section>

        <div className="overview-lists">
          <section className="panel" aria-label="What fails most">
            <header className="panel-head">
              <h2>What fails most</h2>
              <span className="end">
                <Link to={queryLink("/runs/failures", q, win === "24h" ? {} : { window: win })}>
                  Every failure →
                </Link>
              </span>
            </header>
            {fingerprints.length === 0 ? (
              <Empty>Nothing failed in {WINDOW_WORDS[win]}.</Empty>
            ) : (
              <ul className="rank-list">
                {fingerprints.map((g) => (
                  <li key={g.fingerprint}>
                    <Link
                      to={queryLink(
                        "/runs",
                        joinQuery(q, `fingerprint:${g.fingerprint}`, windowTerm("ended", win)),
                      )}
                      title={g.signature}
                    >
                      <span className="rank-name mono">{g.signature}</span>
                      <Meter
                        value={g.runs}
                        max={fingerprints[0]?.runs ?? 1}
                        tone={isCancellation(g.error) ? "neutral" : "critical"}
                      />
                      <span className="num">{formatCount(g.runs)}</span>
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </section>
          <section className="panel" aria-label="Failing actions">
            <header className="panel-head">
              <h2>Failing actions</h2>
              <span className="end">
                <Link to={`/actions${win === "24h" ? "" : `?window=${win}`}`}>Every action →</Link>
              </span>
            </header>
            {failingActions.length === 0 ? (
              <Empty>No call failed in {WINDOW_WORDS[win]}.</Empty>
            ) : (
              <ul className="rank-list">
                {failingActions.map((t) => (
                  <li key={t.action}>
                    <Link
                      to={runsLink(fieldTerm("action", t.action), fieldTerm("error", t.kind))}
                      title={`${t.errors} of ${t.calls} calls failed`}
                    >
                      <span className="rank-name">
                        <span className="mono">{t.action}</span>
                        <span className="muted"> · {t.kind}</span>
                      </span>
                      <Meter value={t.errors} max={failingActions[0]?.errors ?? 1} />
                      <span className="num">{formatCount(t.errors)}</span>
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </div>
      </>
    );
  }

  return (
    <div className="page">
      <PageHeader
        title="Overview"
        info={
          scope.narrowed &&
          !fault && (
            <p>
              The figures count the runs of {scope.q ? <code>{scope.q}</code> : "the organization"}:
              the other terms of the query narrow the runs each figure opens.
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
        facets={current.data?.facets}
        values={lookup.values}
        onTyping={lookup.onTyping}
        fault={fault}
      />
      {body}
    </div>
  );
}
