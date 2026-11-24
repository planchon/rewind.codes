// The runs this run started, from its history: each call of `workflows.<name>.start` that
// answered a run id, with that run's end once it came back as a `rewind:ended:` signal.
import { Link } from "react-router";
import { OutcomeBadge, StatusBadge } from "../../components/Status";
import { shortRunId } from "../../model/format";
import { fieldTerm, queryLink } from "../../model/query";
import type { RunModel } from "../../model/run";
import { startedRuns } from "../../model/workflows";

export function StartedRuns({
  runId,
  model,
  live,
  onSelectEvent,
  onSelectLine,
}: {
  runId: string;
  model: RunModel;
  /** The run goes on: a run whose end has not come back yet is still running. */
  live: boolean;
  onSelectEvent: (idx: number) => void;
  onSelectLine: (line: number) => void;
}) {
  const runs = startedRuns(model);
  if (runs.length === 0) return null;
  return (
    <section className="panel started-runs" aria-label="Runs it started">
      <header className="panel-head">
        <h2>Runs it started</h2>
        <span className="muted num">{runs.length}</span>
        <span className="end">
          <Link to={queryLink("/runs", fieldTerm("started_by", runId))} title="These runs in Runs">
            In Runs
          </Link>
        </span>
      </header>
      <ul className="started-list">
        {runs.map((s) => (
          <li key={s.runId}>
            {s.end?.status ? (
              <StatusBadge status={s.end.status} />
            ) : live ? (
              <OutcomeBadge outcome="open" word="running" />
            ) : (
              <span
                className="muted"
                title="Its end had not come back when this run ended: open it for its state"
              >
                not ended with this run
              </span>
            )}
            <Link to={`/runs/${s.runId}`} title={`The run ${s.runId}`}>
              <span className="mono">{s.workflow}</span>{" "}
              <span className="run-id">{shortRunId(s.runId)}</span>
            </Link>
            {s.detached && (
              <span className="tag" title="Started detached: it lives on its own">
                detached
              </span>
            )}
            <button
              type="button"
              className="link-btn mono"
              onClick={() => onSelectEvent(s.call.idx)}
              title="The call that started it"
            >
              #{s.call.seq} {s.workflow}.start(…)
            </button>
            {s.call.callsite && (
              <button
                type="button"
                className="link-btn mono muted"
                onClick={() => s.call.callsite && onSelectLine(s.call.callsite.line)}
              >
                L{s.call.callsite.line}
              </button>
            )}
            {s.end && (
              <button
                type="button"
                className="link-btn"
                onClick={() => s.end && onSelectEvent(s.end.idx)}
                title="The signal that brought its end"
              >
                end at event #{s.end.idx}
              </button>
            )}
            {s.end?.message && <span className="muted started-message">{s.end.message}</span>}
          </li>
        ))}
      </ul>
    </section>
  );
}
