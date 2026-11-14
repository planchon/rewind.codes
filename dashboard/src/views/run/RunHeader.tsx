// The run's header (status, ids, metadata, labels, exports) and its banners: how it failed, why
// it is quarantined, what it waits for.
import { Download } from "lucide-react";
import { useState } from "react";
import type { JournalState } from "../../api/queries";
import type { Run, Waiting } from "../../api/types";
import { Link, NS_PARAM, useNavigate, useScopedApi } from "../../app/namespace";
import { Ago, CopyButton, Digest, LabelList, RunId, Time, useNow } from "../../components/bits";
import {
  ForkTag,
  MockTag,
  OutcomeBadge,
  QuarantineTag,
  StatusBadge,
} from "../../components/Status";
import { formatCount, formatDuration, formatRelative } from "../../model/format";
import { labelTerm, queryLink } from "../../model/query";
import type { RunModel } from "../../model/run";

function LiveState({ state }: { state: JournalState }) {
  const text = {
    connecting: "connecting…",
    live: "live",
    retrying: "reconnecting…",
    ended: "complete journal",
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
  const api = useScopedApi();
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
      <Download size={13} /> {as === "journal" ? "Journal" : "Cassette"}
    </button>
  );
}

export function RunHeader({
  run,
  model,
  state,
}: {
  run: Run;
  model: RunModel;
  state: JournalState;
}) {
  const now = useNow(10_000);
  const navigate = useNavigate();
  const started = Date.parse(run.createdAt);
  const ended = run.endedAt ? Date.parse(run.endedAt) : null;
  return (
    <header className="run-head">
      <div className="run-title">
        <StatusBadge status={run.status} large />
        <RunId id={run.id} full />
        {run.mode === "mock" && <MockTag />}
        {run.parent && (
          <Link
            to={`/runs/${run.parent.run}`}
            title={`Forked from ${run.parent.run}${run.parent.at != null ? ` at entry ${run.parent.at}` : ""}`}
          >
            <ForkTag>fork of {run.parent.run.slice(0, 8)}…</ForkTag>
          </Link>
        )}
        {run.quarantine && <QuarantineTag />}
        <span className="run-title-end">
          <LiveState state={state} />
          <Link
            className="btn ghost"
            to={queryLink("/runs", `parent:${run.id}`)}
            title="Runs forked from this one"
          >
            Forks
          </Link>
          <ExportButton runId={run.id} as="journal" />
          <ExportButton runId={run.id} as="cassette" />
        </span>
      </div>
      <dl className="run-meta">
        <div>
          <dt>namespace</dt>
          <dd>
            <Link
              className="mono"
              to={`/runs?${new URLSearchParams({ [NS_PARAM]: run.namespace })}`}
              title="The end user the run acts for: their runs"
            >
              {run.namespace}
            </Link>
          </dd>
        </div>
        <div>
          <dt>script</dt>
          <dd>
            <Link to={`/scripts/${run.script}`} title="The script's page">
              <Digest digest={run.script} />
            </Link>
          </dd>
        </div>
        <div>
          <dt>toolset</dt>
          <dd>
            <Link to={`/tools?toolset=${run.toolset}`} title="Statistics of this toolset's tools">
              <Digest digest={run.toolset} />
            </Link>
          </dd>
        </div>
        <div>
          <dt>started</dt>
          <dd>
            <Time ms={started} />{" "}
            <span className="muted">
              (<Ago ms={started} />)
            </span>
          </dd>
        </div>
        <div>
          <dt>{ended ? "took" : "running for"}</dt>
          <dd className="num">{formatDuration((ended ?? now) - started)}</dd>
        </div>
        {!ended && (
          <div>
            <dt>expires</dt>
            <dd className="num" title={run.expiresAt}>
              {formatRelative(Date.parse(run.expiresAt), now)}
            </dd>
          </div>
        )}
        <div>
          <dt>entries</dt>
          <dd className="num">{formatCount(run.historyLen)}</dd>
        </div>
        <div>
          <dt>calls</dt>
          <dd className="num">
            {formatCount(model.calls.length)}
            {model.calls.some((c) => c.end?.outcome === "failed") && (
              <span className="tone-critical">
                {" "}
                · {model.calls.filter((c) => c.end?.outcome === "failed").length} failed
              </span>
            )}
          </dd>
        </div>
        <div>
          <dt>epoch</dt>
          <dd className="num">{run.epoch}</dd>
        </div>
        <div>
          <dt>trace</dt>
          <dd>
            <span className="copyable">
              <span className="digest" title={run.traceId}>
                {run.traceId.slice(0, 12)}…
              </span>
              <CopyButton text={run.traceId} label="Copy trace id" />
            </span>
          </dd>
        </div>
      </dl>
      <div className="run-labels">
        <LabelList
          labels={run.labels}
          onPick={(pair) => {
            const at = pair.indexOf(":");
            navigate(queryLink("/runs", labelTerm(pair.slice(0, at), pair.slice(at + 1))));
          }}
        />
      </div>
    </header>
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
    case "signal":
      return `signal "${w.name}"${w.timeoutAt ? `, gives up ${formatRelative(Date.parse(w.timeoutAt), now)}` : ", no timeout"}`;
  }
}

function waitingHint(w: Waiting): string | null {
  if (w.kind === "tool_call" && w.state === "approval")
    return `Approve: POST /v1/tasks/${w.taskId}/approve · reject: …/reject`;
  if (w.kind === "tool_call" && w.parked === "schema_changed")
    return `The tool no longer matches its pinned schema: approve or reject task ${w.taskId}, fork or cancel the run.`;
  if (w.kind === "tool_call" && w.parked === "credentials_expired")
    return "The server refused the credentials: renew them with PUT /v1/runs/{id}/credentials.";
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

  if (run.error) {
    const e = run.error;
    const failedCall = e.failedCall;
    const call = failedCall ? model.calls.find((c) => c.seq === failedCall.seq) : undefined;
    banners.push(
      <section
        key="e"
        className={`banner ${run.status === "cancelled" ? "tone-neutral-banner" : "tone-critical-banner"}`}
        aria-label="Error"
      >
        <h3>
          <span>
            <span className="mono">{e.name}</span>: {e.message}
          </span>
        </h3>
        <p className="banner-links">
          {e.location && (
            <button
              type="button"
              className="link-btn mono"
              onClick={() => e.location && onSelectLine(e.location.line)}
            >
              script.ts:{e.location.line}:{e.location.column}
            </button>
          )}
          {failedCall && (
            <button
              type="button"
              className="link-btn"
              onClick={() => call && onSelectEntry(call.idx)}
              disabled={!call}
            >
              thrown by{" "}
              <span className="mono">
                #{failedCall.seq} {failedCall.tool}
              </span>
              : {failedCall.error.kind}, {failedCall.error.message}
            </button>
          )}
          {model.cancel && (
            <span className="muted">
              cancellation requested ({model.cancel.reason})
              {model.cancel.message ? `: ${model.cancel.message}` : ""}
            </span>
          )}
        </p>
        {e.frame && <pre className="frame">{e.frame}</pre>}
        {e.stack && (
          <details>
            <summary>Stack</summary>
            <pre className="frame">{e.stack}</pre>
          </details>
        )}
      </section>,
    );
  }

  if (run.abort) {
    banners.push(
      <section key="a" className="banner tone-serious-banner" aria-label="Abort">
        <h3>
          Aborted by the host: <span className="mono">{run.abort.kind}</span>
        </h3>
        <p>{run.abort.message}</p>
      </section>,
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
