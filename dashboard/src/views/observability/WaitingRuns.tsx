// Observability's waiting runs: every run paused on the outside world, grouped by what it waits
// for (a signal, an approval, a timer, a tool), with the line it is paused on and when it wakes.
// The soonest to wake first. A row opens the run at its last step. Live: read again a second after
// the last change, every 10 seconds while the change stream is down.
import { Fragment, type ReactNode } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { useRunLinkState } from "../../app/runLinks";
import { Empty, LabelList, Loading, ProblemView, RunId, useNow } from "../../components/bits";
import { StatusGlyph } from "../../components/Status";
import { formatCount, formatDuration, formatRelative } from "../../model/format";
import { joinQuery, labelTerm, withTerm } from "../../model/query";
import type { RunStats, RunSummary, RunWait, WaitCounts, WaitKind } from "../../model/types";
import { queryFault } from "../../state/errors";
import { useRunSearch } from "../../state/runs";
import { useScript } from "../../state/scripts";
import { TableToolbar } from "../TableTop";

const POLL = 10_000;

type Tab = "all" | "signals" | "approvals" | "timers" | "tools";

const TABS: { tab: Tab; label: string; kinds: readonly WaitKind[] }[] = [
  { tab: "all", label: "All", kinds: [] },
  { tab: "signals", label: "Signals", kinds: ["signal"] },
  { tab: "approvals", label: "Approvals", kinds: ["approval", "parked"] },
  { tab: "timers", label: "Timers", kinds: ["timer"] },
  { tab: "tools", label: "Tools", kinds: ["call", "host", "deferred"] },
];

function readTab(params: URLSearchParams): (typeof TABS)[number] {
  return TABS.find((t) => t.tab === params.get("tab")) ?? (TABS[0] as (typeof TABS)[number]);
}

function tabCount(tab: (typeof TABS)[number], runs: number, waiting: WaitCounts): number {
  if (tab.kinds.length === 0) return runs;
  return tab.kinds.reduce((n, k) => n + waiting[k], 0);
}

const KIND_WORD: Record<WaitKind, string> = {
  signal: "signal",
  timer: "timer",
  call: "tool call",
  host: "host tool",
  approval: "approval",
  parked: "parked",
  deferred: "deferred",
};

/** What a wait is about: a signal's name, a call's tool, a timer. */
function waitTitle(w: RunWait): string {
  switch (w.kind) {
    case "signal":
      return `signal "${w.name ?? "?"}"`;
    case "timer":
      return `timer${w.seq != null ? ` #${w.seq}` : ""}`;
    default:
      return `${w.seq != null ? `#${w.seq} ` : ""}${w.name ?? "a tool"}`;
  }
}

/** The group a wait falls in. */
function groupOf(w: RunWait): string {
  switch (w.kind) {
    case "signal":
      return `signal "${w.name ?? "?"}"`;
    case "timer":
      return "sleep and timers";
    case "approval":
    case "parked":
      return `${w.kind} · ${w.name ?? "a tool"}`;
    default:
      return `${KIND_WORD[w.kind]} · ${w.name ?? "a tool"}`;
  }
}

/** When it wakes, as the operator acts on it. */
function wakeText(w: RunWait, now: number): string {
  if (w.until) {
    const at = formatRelative(Date.parse(w.until), now);
    return w.kind === "signal" ? `gives up ${at}` : at;
  }
  switch (w.kind) {
    case "signal":
      return "when it arrives";
    case "approval":
      return "when approved";
    case "parked":
      return "when released";
    case "deferred":
      return "when its handler answers";
    default:
      return "when the tool answers";
  }
}

/** The line a run is paused on, read from its script once per digest. */
function PausedLine({ digest, line }: { digest: string; line: number }) {
  const script = useScript(digest);
  const text = script.data?.source.split("\n")[line - 1]?.trim();
  return (
    <span className="paused-line" title={text}>
      <b className="num">L{line}</b>
      <code>{text ?? (script.isPending ? "…" : "")}</code>
    </span>
  );
}

interface Row {
  run: RunSummary;
  wait: RunWait;
  more: number;
}

export function WaitingRuns({
  q,
  setQ,
  stats,
}: {
  q: string;
  setQ: (q: string) => void;
  /** The statistics of the waiting runs the query matches, for the tabs' counts. */
  stats: RunStats | undefined;
}) {
  const [params, setParams] = useSearchParams();
  const tab = readTab(params);
  const navigate = useNavigate();
  const linkState = useRunLinkState();
  const now = useNow(10_000);
  const tabTerm = tab.kinds.length === 0 ? "waiting:*" : `waiting:${tab.kinds.join(",")}`;
  const runs = useRunSearch({
    q: joinQuery(q, tabTerm),
    sort: "wakes",
    pageSize: 100,
    pollMs: POLL,
  });
  const fault = queryFault(runs.error);

  const rows: Row[] = [];
  for (const run of runs.runs) {
    const waits = (run.waitingFor ?? []).filter(
      (w) => tab.kinds.length === 0 || tab.kinds.includes(w.kind),
    );
    const wait = waits[0];
    if (wait) rows.push({ run, wait, more: (run.waitingFor ?? []).length - 1 });
  }
  const groups = new Map<string, Row[]>();
  for (const row of rows) {
    const key = groupOf(row.wait);
    const list = groups.get(key);
    if (list) list.push(row);
    else groups.set(key, [row]);
  }

  const groupMeta = (list: Row[]) => {
    const wakes = list.flatMap((r) => (r.wait.until ? [Date.parse(r.wait.until)] : []));
    const oldest = Math.min(...list.map((r) => Date.parse(r.wait.since)));
    const n = `${formatCount(list.length)} run${list.length === 1 ? "" : "s"}`;
    if (wakes.length > 0) return `${n} · next wakes ${formatRelative(Math.min(...wakes), now)}`;
    return `${n} · oldest waiting ${formatDuration(now - oldest)}`;
  };

  let body: ReactNode;
  if (fault) {
    body = (
      <Empty>Fix the query to see the waiting runs: the term at fault is underlined above.</Empty>
    );
  } else if (runs.status === "pending") {
    body = <Loading>Loading the waiting runs…</Loading>;
  } else if (runs.status === "error") {
    body = <ProblemView error={runs.error} title="The waiting runs could not be read" />;
  } else if (rows.length === 0) {
    body = (
      <Empty>
        No run waits {tab.tab === "all" ? "for anything" : `on ${tab.label.toLowerCase()}`} now.
      </Empty>
    );
  } else {
    body = (
      <div className={`table-wrap${runs.stale ? " is-stale" : ""}`}>
        <table className="grid waiting-table">
          <thead>
            <tr>
              <th>Run</th>
              <th>Waiting for</th>
              <th className="right">Since</th>
              <th>Wakes</th>
              <th>Paused at</th>
            </tr>
          </thead>
          <tbody>
            {[...groups.entries()].map(([group, list]) => (
              <Fragment key={group}>
                <tr className="group-row">
                  <td colSpan={5}>
                    <span className="mono">{group}</span>
                    <span className="muted"> {groupMeta(list)}</span>
                  </td>
                </tr>
                {list.map(({ run, wait, more }) => (
                  <tr
                    key={run.id}
                    className="link"
                    onClick={(e) => {
                      if ((e.target as HTMLElement).closest("a, button")) return;
                      void navigate(`/runs/${run.id}`, { state: linkState });
                    }}
                  >
                    <td className="wrap waiting-run">
                      <span className="labels">
                        <span className="status active">
                          <StatusGlyph status="active" size={12} />
                        </span>
                        <Link
                          to={`/runs/${run.id}`}
                          state={linkState}
                          className="row-link"
                          title="Open the run at its last step"
                        >
                          <RunId id={run.id} />
                        </Link>
                        <span className="headline mono" title={run.headline}>
                          {run.headline}
                        </span>
                      </span>
                      <LabelList
                        labels={run.labels}
                        onPick={(pair) => {
                          const at = pair.indexOf(":");
                          setQ(withTerm(q, labelTerm(pair.slice(0, at), pair.slice(at + 1))));
                        }}
                      />
                    </td>
                    <td>
                      <span className={`wait-kind kind-${wait.kind}`}>{KIND_WORD[wait.kind]}</span>{" "}
                      <span className="mono">{waitTitle(wait)}</span>
                      {more > 0 && <span className="muted"> +{more} more</span>}
                    </td>
                    <td className="right num" title={wait.since}>
                      {formatDuration(Math.max(0, now - Date.parse(wait.since)))}
                    </td>
                    <td className="num" title={wait.until ?? undefined}>
                      {wakeText(wait, now)}
                    </td>
                    <td>
                      {wait.callsite ? (
                        <PausedLine digest={run.script} line={wait.callsite.line} />
                      ) : (
                        <span className="muted">no callsite</span>
                      )}
                    </td>
                  </tr>
                ))}
              </Fragment>
            ))}
          </tbody>
        </table>
        {runs.hasMore && (
          <div className="list-foot">
            <button
              type="button"
              className="btn small"
              disabled={runs.loadingMore}
              onClick={runs.loadMore}
            >
              {runs.loadingMore ? "Loading…" : "Load more"}
            </button>
          </div>
        )}
      </div>
    );
  }

  return (
    <div className="stack">
      <div className="tabs view-tabs" role="tablist" aria-label="What the runs wait for">
        {TABS.map((t) => (
          <button
            key={t.tab}
            type="button"
            role="tab"
            aria-selected={t.tab === tab.tab}
            onClick={() =>
              setParams((current) => {
                const next = new URLSearchParams(current);
                if (t.tab === "all") next.delete("tab");
                else next.set("tab", t.tab);
                return next;
              })
            }
          >
            {t.label}{" "}
            <span className="count">
              {stats ? formatCount(tabCount(t, stats.runs, stats.waiting)) : "–"}
            </span>
          </button>
        ))}
      </div>
      <div role="tabpanel" className="run-table">
        <TableToolbar pending={runs.pending} onShow={runs.showPending} setAtTop={runs.setAtTop} />
        {body}
      </div>
    </div>
  );
}
