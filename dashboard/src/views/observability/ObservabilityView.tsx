// Observability: the runs, looked at and filtered (archi/05). Four views of the same query: every
// run, the active ones, those waiting on the outside world (grouped by what they wait for), and
// the failures (grouped by fingerprint). Its sidebar holds the views, the saved searches and the
// filters; the query lives in the URL, and a filter is a term of it.
import { Menu } from "@base-ui/react/menu";
import {
  Activity,
  Bookmark,
  ChevronDown,
  CircleAlert,
  Hourglass,
  List,
  RefreshCw,
  X,
} from "lucide-react";
import { type ReactNode, useMemo } from "react";
import { Link, Navigate, useParams, useSearchParams } from "react-router";
import { queryFault } from "../../api/client";
import { useRunStats, useRuns } from "../../api/queries";
import type { RunSort, RunStats, RunStatus } from "../../api/types";
import { RunsFrom } from "../../app/runLinks";
import { type SearchView, savedSearchPath, useSavedSearches } from "../../app/searches";
import { useSettings } from "../../app/settings";
import { PageSidebar } from "../../app/sidebar";
import { Empty } from "../../components/bits";
import { StackedBars } from "../../components/Charts";
import { PageHeader } from "../../components/PageHeader";
import { QueryBar } from "../../components/QueryBar";
import { SaveSearch } from "../../components/SaveSearch";
import { RANGES, type RangeKey } from "../../model/filters";
import { formatCount, formatTime } from "../../model/format";
import {
  fieldTerm,
  joinQuery,
  labelTerm,
  queryLink,
  queryTime,
  toggleFacet,
  withTerm,
} from "../../model/query";
import { Seg } from "../FilterBits";
import { RunTable } from "../RunTable";
import { BUCKETS, useRunQueryValues, useSearchQuery, useWindow } from "../search";
import { FailureGroups } from "./FailureGroups";
import { FilterGroups, TimelineSelect, timelineOf, withTimeline } from "./Filters";
import { WaitingRuns } from "./WaitingRuns";

type ViewKey = "all" | "active" | "waiting" | "failures";

interface View {
  key: ViewKey;
  path: string;
  label: string;
  /** The term the view adds to the query. */
  term: string | null;
  saved: SearchView;
  icon: ReactNode;
}

const VIEWS: readonly View[] = [
  {
    key: "all",
    path: "/observability",
    label: "All runs",
    term: null,
    saved: "runs",
    icon: <List size={14} />,
  },
  {
    key: "active",
    path: "/observability/active",
    label: "Active",
    term: "status:active",
    saved: "active",
    icon: <Activity size={14} />,
  },
  {
    key: "waiting",
    path: "/observability/waiting",
    label: "Waiting",
    term: "waiting:*",
    saved: "waiting",
    icon: <Hourglass size={14} />,
  },
  {
    key: "failures",
    path: "/observability/failures",
    label: "Failures",
    term: "status:failed,aborted,cancelled",
    saved: "failures",
    icon: <CircleAlert size={14} />,
  },
];

const SORTS: { value: RunSort; label: string; title: string }[] = [
  { value: "started", label: "Started", title: "The most recently started first" },
  { value: "activity", label: "Activity", title: "The most recent activity first" },
  { value: "wakes", label: "Wakes", title: "The soonest to wake by itself first" },
];

function readSort(params: URLSearchParams): RunSort {
  const sort = params.get("sort");
  return sort === "activity" || sort === "wakes" ? sort : "started";
}

/** How many runs each view shows, for the query. */
function viewCount(view: ViewKey, all: RunStats | undefined, waiting: RunStats | undefined) {
  if (view === "waiting") return waiting?.runs;
  if (!all) return undefined;
  const s = all.statuses;
  if (view === "all") return all.runs;
  if (view === "active") return s.active;
  return s.failed + s.aborted + s.cancelled;
}

function ViewSelect({
  current,
  q,
  counts,
}: {
  current: View;
  q: string;
  counts: Record<ViewKey, number | undefined>;
}) {
  const count = counts[current.key];
  return (
    <Menu.Root>
      <Menu.Trigger className="view-select" aria-label={`View: ${current.label}`}>
        <span className="view-select-icon" aria-hidden="true">
          {current.icon}
        </span>
        <span className="view-select-label">{current.label}</span>
        {count !== undefined && <span className="count">{formatCount(count)}</span>}
        <ChevronDown size={14} className="view-select-chevron" aria-hidden="true" />
      </Menu.Trigger>
      <Menu.Portal>
        <Menu.Positioner className="menu-positioner" side="bottom" align="start" sideOffset={4}>
          <Menu.Popup className="menu-popup view-menu">
            {VIEWS.map((v) => (
              <Menu.LinkItem
                key={v.key}
                className="menu-item"
                closeOnClick
                aria-current={v.key === current.key ? "page" : undefined}
                render={<Link to={queryLink(v.path, q)} />}
              >
                <span className="menu-item-icon" aria-hidden="true">
                  {v.icon}
                </span>
                <span className="menu-item-text">{v.label}</span>
                {counts[v.key] !== undefined && (
                  <span className="menu-item-end num">{formatCount(counts[v.key] ?? 0)}</span>
                )}
              </Menu.LinkItem>
            ))}
          </Menu.Popup>
        </Menu.Positioner>
      </Menu.Portal>
    </Menu.Root>
  );
}

function SavedSearches() {
  const { searches, remove } = useSavedSearches();
  return (
    <section className="pane-section" aria-label="Saved searches">
      <h2 className="pane-label">Saved searches</h2>
      {searches.length === 0 ? (
        <p className="pane-hint">Save a query to find it here, and in Find.</p>
      ) : (
        <ul className="saved-list">
          {searches.map((s) => (
            <li key={s.name}>
              <Link to={savedSearchPath(s)} className="saved-item" title={s.q || "every run"}>
                <Bookmark size={14} aria-hidden="true" />
                <span className="saved-name">{s.name}</span>
              </Link>
              <button
                type="button"
                className="icon-btn saved-remove"
                aria-label={`Forget the saved search ${s.name}`}
                onClick={() => remove(s.name)}
              >
                <X size={12} />
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

/** The runs a query matches over a window, by status, each segment a link to its runs. */
function RunsHistogram({ q, win }: { q: string; win: RangeKey }) {
  const { zone } = useSettings();
  const width = RANGES[win];
  const { bucket, ms: bucketMs } = BUCKETS[win];
  const base = withTimeline(q, null);
  const stats = useRunStats(
    `obs-histogram|${win}|${base}`,
    () => {
      const now = Date.now();
      return {
        ...(base ? { q: base } : {}),
        since: new Date(now - width).toISOString(),
        until: new Date(now).toISOString(),
        bucket,
        top: 1,
      };
    },
    15_000,
  );
  if (!stats.data) return <div className="histogram placeholder" aria-hidden="true" />;
  const since = stats.data.since ? Date.parse(stats.data.since) : Date.now() - width;
  const buckets = (stats.data.series ?? []).map((b) => {
    const from = Date.parse(b.at);
    return { from, to: from + bucketMs, counts: b.statuses };
  });
  if (buckets.length === 0) return null;
  const tick = (ms: number) => {
    const t = formatTime(ms, zone);
    return win === "1h" || win === "24h" ? t.slice(11, 16) : t.slice(5, 10);
  };
  return (
    <div className="histogram">
      <StackedBars
        buckets={buckets}
        title="Runs started"
        height={72}
        legend={false}
        tick={tick}
        bucketLabel={(b) => `${formatTime(b.from, zone).slice(0, 16)} – ${tick(b.to)}`}
        href={(b, status: RunStatus) =>
          queryLink(
            "/observability",
            joinQuery(
              base,
              `status:${status}`,
              `started:${queryTime(Math.max(b.from, since))}..${queryTime(b.to)}`,
            ),
          )
        }
      />
    </div>
  );
}

function RunsList({ q, view, setQ }: { q: string; view: View; setQ: (q: string) => void }) {
  const [params] = useSearchParams();
  const sort = readSort(params);
  const asked = joinQuery(q, view.term);
  const runs = useRuns(`${view.key}|${sort}|${q}`, () => ({
    ...(asked ? { q: asked } : {}),
    ...(sort !== "started" ? { sort } : {}),
    limit: 50,
  }));
  const add = (term: string) => setQ(withTerm(q, term));
  if (queryFault(runs.error))
    return <Empty>Fix the query to see its runs: the term at fault is underlined above.</Empty>;
  return (
    <RunTable
      query={runs}
      onPickLabel={(pair) => {
        const at = pair.indexOf(":");
        add(labelTerm(pair.slice(0, at), pair.slice(at + 1)));
      }}
      onPickScript={(digest) => add(fieldTerm("script", digest))}
      showNamespace
      onPickNamespace={(namespace) => add(fieldTerm("namespace", namespace))}
      empty={
        q
          ? "No run matches this query."
          : view.key === "active"
            ? "No run is active now."
            : "No run yet."
      }
    />
  );
}

export function ObservabilityView() {
  const { view: raw } = useParams();
  const view = VIEWS.find((v) => v.key === (raw ?? "all"));
  const { q, setQ } = useSearchQuery();
  const lookup = useRunQueryValues();
  const [params, setParams] = useSearchParams();
  const [win, setWin] = useWindow();

  // The counts of the views, and the facets of the one shown.
  const all = useRunStats(`obs|all|${q}`, () => ({ ...(q ? { q } : {}), top: 10 }), 15_000);
  const waiting = useRunStats(
    `obs|waiting|${q}`,
    () => ({ q: joinQuery(q, "waiting:*"), top: 10 }),
    10_000,
  );
  const shown = useRunStats(
    `obs|${view?.key ?? "all"}|${q}`,
    () => ({ q: joinQuery(q, view?.term), top: 10 }),
    15_000,
  );
  const counts = useMemo(
    () => ({
      all: viewCount("all", all.data, waiting.data),
      active: viewCount("active", all.data, waiting.data),
      waiting: viewCount("waiting", all.data, waiting.data),
      failures: viewCount("failures", all.data, waiting.data),
    }),
    [all.data, waiting.data],
  );

  if (!view) return <Navigate to="/observability" replace />;
  const fault = queryFault(shown.error) ?? queryFault(all.error);
  const sort = readSort(params);
  const timeline = view.key === "failures" ? win : timelineOf(q);

  return (
    <RunsFrom>
      <div className="page">
        <PageSidebar title="Observability" back={null} backLabel="Show the views">
          <ViewSelect current={view} q={q} counts={counts} />
          <SavedSearches />
          <section className="pane-section" aria-label="Filters">
            <h2 className="pane-label">Filters</h2>
            {view.key === "failures" ? (
              <TimelineSelect value={win} onChange={(w) => setWin(w ?? "24h")} any={false} />
            ) : (
              <TimelineSelect value={timeline} onChange={(w) => setQ(withTimeline(q, w))} />
            )}
            <FilterGroups
              facets={fault ? undefined : shown.data?.facets}
              q={q}
              onToggle={(field, value) => setQ(toggleFacet(q, field, value))}
            />
          </section>
        </PageSidebar>
        <PageHeader
          crumbs="Observability"
          title={view.label}
          actions={
            <>
              <SaveSearch q={q} view={view.saved} />
              <button
                type="button"
                className="btn icon-only"
                aria-label="Refresh"
                title="Refresh"
                onClick={() => {
                  void all.refetch();
                  void waiting.refetch();
                  void shown.refetch();
                }}
              >
                <RefreshCw size={14} className={shown.isFetching ? "spin" : undefined} />
              </button>
            </>
          }
        />
        <div className="page-body">
          <QueryBar
            value={q}
            onSubmit={setQ}
            facets={shown.data?.facets}
            values={lookup.values}
            onTyping={lookup.onTyping}
            fault={fault}
            placeholder='@agent:triage status:failed tool:github.* error:timeout "rate limit"'
          >
            {(view.key === "all" || view.key === "active") && (
              <Seg<RunSort>
                label="Order"
                value={sort}
                onChange={(v) =>
                  setParams((current) => {
                    const next = new URLSearchParams(current);
                    if (v === "started") next.delete("sort");
                    else next.set("sort", v);
                    return next;
                  })
                }
                options={SORTS}
              />
            )}
          </QueryBar>
          {(view.key === "all" || view.key === "active") && (
            <>
              {!fault && <RunsHistogram q={q} win={timeline ?? "24h"} />}
              <RunsList q={q} view={view} setQ={setQ} />
            </>
          )}
          {view.key === "waiting" &&
            (fault ? (
              <Empty>
                Fix the query to see the waiting runs: the term at fault is underlined above.
              </Empty>
            ) : (
              <WaitingRuns q={q} setQ={setQ} stats={waiting.data} />
            ))}
          {view.key === "failures" && <FailureGroups q={q} setQ={setQ} win={win} />}
        </div>
      </div>
    </RunsFrom>
  );
}
