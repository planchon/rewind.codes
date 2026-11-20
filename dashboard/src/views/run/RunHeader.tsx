// The run's card, details and actions (in its sidebar and its header), and its banners: how it
// ended (RunEnd.tsx), why it is quarantined, what it waits for.
import { Download, GitFork } from "lucide-react";
import { useState } from "react";
import { Link, useNavigate } from "react-router";
import type { JournalState } from "../../api/queries";
import type { Run, Waiting } from "../../api/types";
import { useApi } from "../../app/session";
import { Ago, CopyButton, Digest, LabelList, RunId, useNow } from "../../components/bits";
import { NamespaceName } from "../../components/NamespaceName";
import { MockTag, OutcomeBadge, QuarantineTag, StatusBadge } from "../../components/Status";
import { formatCount, formatDuration, formatRelative, shortRunId } from "../../model/format";
import { fieldTerm, labelTerm, queryLink } from "../../model/query";
import type { RunModel } from "../../model/run";
import { endedRunOf, workflowRef } from "../../model/workflows";
import { endingOf, RunEndBanner } from "./RunEnd";

function LiveState({ state }: { state: JournalState }) {
  const text = {
    connecting: "connecting…",
    live: "live",
    retrying: "reconnecting…",
    ended: "",
    error: "stream failed",
  }[state];
  return (
    <span
      className={`live-state state-${state}`}
      title="The journal, read from the run's event stream"
    >
      <span className={`dot${state === "live" ? " pulse" : ""}`} aria-hidden="true" />
      {text}
    </span>
  );
}

function ExportButton({ runId, as }: { runId: string; as: "journal" | "cassette" }) {
  const api = useApi();
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState<string | null>(null);
  return (
    <button
      type="button"
      className="btn"
      disabled={busy}
      title={
        failed ??
        (as === "journal"
          ? "The journal document, stored format v1"
          : "A cassette of its tool calls, for rewind test")
      }
      onClick={async () => {
        setBusy(true);
        setFailed(null);
        try {
          const blob = await api.journal(runId, as);
          const url = URL.createObjectURL(blob);
          const a = document.createElement("a");
          a.href = url;
          a.download = `${runId}.${as}.json`;
          a.click();
          setTimeout(() => URL.revokeObjectURL(url), 1_000);
        } catch (e) {
          setFailed(e instanceof Error ? e.message : String(e));
        } finally {
          setBusy(false);
        }
      }}
    >
      <Download size={14} aria-hidden="true" /> {as === "journal" ? "Journal" : "Cassette"}
    </button>
  );
}

/** The run at the head of its sidebar: its status and its id, on one line. Tags follow only
 * when they say something: the stream while the run goes on, a mock run, a quarantine. When it
 * started and how long it lasted are in its details below. */
export function RunCard({ run, state }: { run: Run; state: JournalState }) {
  const live = state !== "ended";
  return (
    <div className="run-ident">
      <div className="run-ident-line">
        <StatusBadge status={run.status} />
        <RunId id={run.id} />
      </div>
      {(live || run.mode === "mock" || run.quarantine) && (
        <div className="run-ident-tags">
          {live && <LiveState state={state} />}
          {run.mode === "mock" && <MockTag />}
          {run.quarantine && <QuarantineTag />}
        </div>
      )}
    </div>
  );
}

/** The actions of the run's page: its forks, its journal and its cassette. */
export function RunActions({ run }: { run: Run }) {
  return (
    <>
      <Link
        className="btn"
        to={queryLink("/observability", `parent:${run.id}`)}
        title="Runs forked from this one"
      >
        <GitFork size={14} aria-hidden="true" /> Forks
      </Link>
      <ExportButton runId={run.id} as="journal" />
      <ExportButton runId={run.id} as="cassette" />
    </>
  );
}

/** The run's details, in its sidebar whatever the section. */
export function RunDetails({ run, model }: { run: Run; model: RunModel }) {
  const now = useNow(10_000);
  const navigate = useNavigate();
  const started = Date.parse(run.createdAt);
  const ended = run.endedAt ? Date.parse(run.endedAt) : null;
  const failedCalls = model.calls.filter((c) => c.end?.outcome === "failed").length;
  return (
    <dl className="run-details">
      <div>
        <dt>{ended ? "Duration" : "Running for"}</dt>
        <dd className="num">{formatDuration((ended ?? now) - started)}</dd>
      </div>
      <div>
        <dt>Started</dt>
        <dd>
          <Ago ms={started} />
        </dd>
      </div>
      {!ended && (
        <div>
          <dt>Expires</dt>
          <dd className="num" title={run.expiresAt}>
            {formatRelative(Date.parse(run.expiresAt), now)}
          </dd>
        </div>
      )}
      <div>
        <dt>Namespace</dt>
        <dd>
          <Link
            to={queryLink("/observability", fieldTerm("namespace", run.namespace))}
            title="The end user the run acts for: their runs"
          >
            <NamespaceName name={run.namespace} />
          </Link>
        </dd>
      </div>
      {run.workflow && (
        <div>
          <dt>Workflow</dt>
          <dd>
            <Link
              to={queryLink("/observability", fieldTerm("workflow", run.workflow.name))}
              className="mono"
              title={`Version ${run.workflow.version}. The runs of this workflow`}
            >
              {workflowRef(run.workflow)}
            </Link>
          </dd>
        </div>
      )}
      {run.startedBy && (
        <div>
          <dt>Started by</dt>
          <dd>
            <Link
              to={`/runs/${run.startedBy.run}`}
              title={`Started by the run ${run.startedBy.run}, at its call #${run.startedBy.seq}${
                run.startedBy.detached
                  ? ": detached, it lives on its own"
                  : ": that run does not end before this one, and cancels it if it fails"
              }`}
            >
              <span className="mono">{shortRunId(run.startedBy.run)}</span>
              {run.startedBy.detached && " · detached"}
            </Link>
          </dd>
        </div>
      )}
      {run.parent && (
        <div>
          <dt>Fork of</dt>
          <dd>
            <Link
              to={`/runs/${run.parent.run}`}
              title={`Forked from ${run.parent.run}${run.parent.at != null ? ` at entry ${run.parent.at}` : ""}`}
            >
              <span className="mono">{shortRunId(run.parent.run)}</span>
            </Link>
          </dd>
        </div>
      )}
      <div>
        <dt>Script</dt>
        <dd>
          <Link to={`/scripts/${run.script}`} title="The script's page">
            <Digest digest={run.script} />
          </Link>
        </dd>
      </div>
      <div>
        <dt>Toolset</dt>
        <dd>
          <Link
            to={queryLink("/tools", `toolset:${run.toolset}`)}
            title="Statistics of this toolset's tools"
          >
            <Digest digest={run.toolset} />
          </Link>
        </dd>
      </div>
      <div>
        <dt>Calls</dt>
        <dd className="num">
          {formatCount(model.calls.length)}
          {failedCalls > 0 && <span className="tone-critical"> · {failedCalls} failed</span>}
        </dd>
      </div>
      <div>
        <dt>Entries · epoch</dt>
        <dd className="num">
          {formatCount(run.historyLen)} · {run.epoch}
        </dd>
      </div>
      <div>
        <dt>Trace</dt>
        <dd>
          <span className="copyable">
            <span className="digest" title={run.traceId}>
              {run.traceId.slice(0, 12)}…
            </span>
            <CopyButton text={run.traceId} label="Copy trace id" />
          </span>
        </dd>
      </div>
      {Object.keys(run.labels).length > 0 && (
        <div className="run-details-labels">
          <dt>Labels</dt>
          <dd>
            <LabelList
              labels={run.labels}
              onPick={(pair) => {
                const at = pair.indexOf(":");
                void navigate(
                  queryLink("/observability", labelTerm(pair.slice(0, at), pair.slice(at + 1))),
                );
              }}
            />
          </dd>
        </div>
      )}
    </dl>
  );
}

function waitingText(w: Waiting, now: number): string {
  switch (w.kind) {
    case "tool_call": {
      const attempt = w.attempt > 1 ? `, attempt ${w.attempt}` : "";
      const state = {
        approval: "waits for the harness's approval",
        ready: "is queued for a worker",
        claimed: "is in flight",
        deferred: "was deferred by its handler",
        parked: `is parked (${(w.parked ?? "parked").replace("_", " ")})`,
        done: "is done",
      }[w.state];
      return `#${w.seq} ${w.tool} ${state}${attempt}`;
    }
    case "timer":
      return `timer #${w.seq} fires ${formatRelative(Date.parse(w.fireAt), now)}`;
    case "signal": {
      const ended = endedRunOf(w.name);
      const what =
        ended === null ? `signal "${w.name}"` : `the end of run ${shortRunId(ended)} (result())`;
      return `${what}${w.timeoutAt ? `, gives up ${formatRelative(Date.parse(w.timeoutAt), now)}` : ", no timeout"}`;
    }
  }
}

function waitingHint(w: Waiting): string | null {
  if (w.kind === "tool_call" && w.state === "approval")
    return `Approve: POST /v1/tasks/${w.taskId}/approve · reject: …/reject`;
  if (w.kind === "tool_call" && w.parked === "schema_changed")
    return `The tool no longer matches its pinned schema: approve or reject task ${w.taskId}, fork or cancel the run.`;
  if (w.kind === "tool_call" && w.parked === "credentials_expired")
    return "The server refused the credentials: renew them with PUT /v1/runs/{id}/credentials.";
  if (w.kind === "signal" && endedRunOf(w.name) !== null) return null;
  if (w.kind === "signal") return `Send it: POST /v1/runs/{id}/signals/${w.name}`;
  return null;
}

/** The activity a waiting item is about, to select it. */
function waitingIdx(w: Waiting, model: RunModel): number | null {
  if (w.kind === "tool_call") return model.calls.find((c) => c.seq === w.seq)?.idx ?? null;
  if (w.kind === "timer") return model.timers.find((t) => t.seq === w.seq)?.idx ?? null;
  return model.signals.find((s) => s.name === w.name && s.end === null)?.idx ?? null;
}

export function RunBanners({
  run,
  model,
  onSelectEntry,
  onSelectLine,
}: {
  run: Run;
  model: RunModel;
  onSelectEntry: (idx: number) => void;
  onSelectLine: (line: number) => void;
}) {
  const now = useNow(10_000);
  const banners = [];

  if (run.quarantine) {
    banners.push(
      <section key="q" className="banner tone-warning-banner" aria-label="Quarantine">
        <h3>
          <QuarantineTag />{" "}
          {run.quarantine.cause === "non_determinism"
            ? "Its replay diverged from the journal"
            : "Its activations kept failing"}
        </h3>
        <p className="mono">{run.quarantine.reason}</p>
        <p className="hint">
          Nothing runs until an operator acts: <code>rewind release {run.id}</code> to retry, or{" "}
          <code>rewind release {run.id} --give-up</code> to abort it.
        </p>
      </section>,
    );
  }

  const ending = endingOf(run, model);
  if (ending && ending.kind !== "completed") {
    banners.push(
      <RunEndBanner
        key="end"
        run={run}
        ending={ending}
        model={model}
        onSelectEntry={onSelectEntry}
        onSelectLine={onSelectLine}
      />,
    );
  }

  const waiting = run.status === "active" ? (run.waitingFor ?? []) : [];
  if (waiting.length > 0 || (model.cancel && run.status === "active")) {
    banners.push(
      <section key="w" className="banner tone-info-banner" aria-label="Waiting for">
        <h3>Waiting for</h3>
        <ul className="waiting">
          {waiting.map((w) => {
            const idx = waitingIdx(w, model);
            const hint = waitingHint(w);
            return (
              <li key={`${w.kind}-${w.kind === "signal" ? w.name : w.seq}`}>
                <OutcomeBadge outcome="open" word="" />
                <button
                  type="button"
                  className="link-btn"
                  disabled={idx === null}
                  onClick={() => idx !== null && onSelectEntry(idx)}
                >
                  {waitingText(w, now)}
                </button>
                {w.callsite && <span className="muted mono">L{w.callsite.line}</span>}
                {hint && <span className="hint">{hint.replace("{id}", run.id)}</span>}
              </li>
            );
          })}
          {model.cancel && run.status === "active" && (
            <li>
              <OutcomeBadge outcome="open" word="" />
              Cancellation requested ({model.cancel.reason}): the script has one activation to clean
              up.
            </li>
          )}
        </ul>
      </section>,
    );
  }

  return banners.length > 0 ? <div className="banners">{banners}</div> : null;
}
