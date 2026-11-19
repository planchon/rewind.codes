// Observability's failures: the failed, cancelled and aborted runs of a window, grouped by
// fingerprint (the error's name and its message with what varies set aside), across the many
// scripts models write. A group opens its runs; its latest run opens on the step that failed.
import type { ReactNode } from "react";
import { Link, useNavigate } from "react-router";
import { queryFault } from "../../api/client";
import { useFailures } from "../../api/queries";
import { useRunLinkState } from "../../app/runLinks";
import { Ago, Empty, Loading, ProblemView, RunId } from "../../components/bits";
import { Sparkline } from "../../components/Charts";
import { StatusBadge } from "../../components/Status";
import { isCancellation } from "../../model/ending";
import { RANGES, type RangeKey } from "../../model/filters";
import { formatCount, formatDuration, splitLabel } from "../../model/format";
import { joinQuery, labelTerm, queryLink } from "../../model/query";
import { BUCKETS, WINDOW_WORDS, windowTerm } from "../search";

export function FailureGroups({
  q,
  setQ,
  win,
}: {
  q: string;
  setQ: (q: string) => void;
  win: RangeKey;
}) {
  const navigate = useNavigate();
  const linkState = useRunLinkState();
  const width = RANGES[win];
  const { bucket, ms: bucketMs } = BUCKETS[win];
  const failures = useFailures(
    `failures|${win}|${q}`,
    () => {
      const now = Date.now();
      return {
        ...(q ? { q } : {}),
        since: new Date(now - width).toISOString(),
        until: new Date(now).toISOString(),
        bucket,
        limit: 100,
      };
    },
    30_000,
  );
  const fault = queryFault(failures.error);
  const groupLink = (fingerprint: string) =>
    queryLink(
      "/observability",
      joinQuery(q, `fingerprint:${fingerprint}`, windowTerm("ended", win)),
    );

  let body: ReactNode;
  if (fault) {
    body = <Empty>Fix the query to see the failures: the term at fault is underlined above.</Empty>;
  } else if (failures.isPending) {
    body = <Loading>Loading the failures…</Loading>;
  } else if (failures.isError) {
    body = <ProblemView error={failures.error} title="The failures could not be read" />;
  } else if (failures.data.groups.length === 0) {
    body = <Empty>No run failed, was cancelled or aborted in {WINDOW_WORDS[win]}.</Empty>;
  } else {
    const page = failures.data;
    body = (
      <div className="table-wrap">
        <table className="grid failures-table">
          <thead>
            <tr>
              <th>Signature</th>
              <th>Error</th>
              <th className="right">Runs</th>
              <th title={`Runs that ended, by ${formatDuration(bucketMs)}`}>{win}</th>
              <th className="right">Scripts</th>
              <th>Seen</th>
              <th title="The line of script.ts most of its runs ended at">Line</th>
              <th>Labels</th>
              <th>Latest</th>
            </tr>
          </thead>
          <tbody>
            {page.groups.map((g) => (
              <tr
                key={g.fingerprint}
                className="link"
                onClick={(e) => {
                  if ((e.target as HTMLElement).closest("a, button")) return;
                  void navigate(groupLink(g.fingerprint));
                }}
              >
                <td className="wrap signature-cell">
                  <Link
                    to={groupLink(g.fingerprint)}
                    className="row-link mono"
                    title={`fingerprint:${g.fingerprint}`}
                  >
                    {g.signature}
                  </Link>
                  <span className="muted ellipsis" title={g.latest.message}>
                    {g.latest.message}
                  </span>
                </td>
                <td>
                  <span className={`mono run-error${isCancellation(g.error) ? " cancelled" : ""}`}>
                    {g.error}
                  </span>
                </td>
                <td className="right num">
                  <Link to={groupLink(g.fingerprint)}>{formatCount(g.runs)}</Link>
                </td>
                <td>
                  <Sparkline
                    series={g.series ?? []}
                    label={`Runs of ${g.error} by ${formatDuration(bucketMs)}`}
                  />
                </td>
                <td className="right num">{formatCount(g.scripts)}</td>
                <td className="dim seen-cell">
                  <span>
                    first <Ago ms={Date.parse(g.firstEndedAt)} />
                  </span>
                  <span>
                    last <Ago ms={Date.parse(g.lastEndedAt)} />
                  </span>
                </td>
                <td className="num">
                  {g.line != null ? `L${g.line}` : <span className="muted">–</span>}
                </td>
                <td className="wrap">
                  <span className="labels">
                    {g.labels.slice(0, 3).map((l) => {
                      const pair = splitLabel(l.value);
                      return (
                        <button
                          key={l.value}
                          type="button"
                          className="label"
                          title={`Only the failures of ${l.value}`}
                          onClick={() => pair && setQ(joinQuery(q, labelTerm(pair[0], pair[1])))}
                        >
                          <span className="k">{pair ? `${pair[0]}:` : ""}</span>
                          {pair ? pair[1] : l.value}
                          <span className="count">{formatCount(l.runs)}</span>
                        </button>
                      );
                    })}
                  </span>
                </td>
                <td>
                  <span className="labels">
                    <StatusBadge status={g.latest.status} />
                    <Link
                      to={`/runs/${g.latest.id}`}
                      state={linkState}
                      title="The run that ended last, on the step that failed"
                    >
                      <RunId id={g.latest.id} />
                    </Link>
                  </span>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {page.totalGroups > page.groups.length && (
          <div className="list-foot">
            {formatCount(page.groups.length)} of {formatCount(page.totalGroups)} signatures shown
          </div>
        )}
      </div>
    );
  }

  return (
    <div className="stack">
      <p className="view-note muted">
        {failures.data
          ? `${formatCount(failures.data.runs)} runs, ${formatCount(failures.data.totalGroups)} signatures, ended in ${WINDOW_WORDS[win]}`
          : WINDOW_WORDS[win]}
      </p>
      {body}
    </div>
  );
}
