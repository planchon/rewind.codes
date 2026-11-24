// The run's tags and controls (in its header), its details (in its sidebar), and its banners:
// how it ended (RunEnd.tsx), why it is quarantined, what it waits for.
import { Download, GitFork } from "lucide-react";
import { useState } from "react";
import { Link, useNavigate } from "react-router";
import { runPath } from "../../app/runLinks";
import {
  Ago,
  CopyButton,
  Digest,
  LabelList,
  RunHandle,
  RunId,
  useNow,
} from "../../components/bits";
import { NamespaceName } from "../../components/NamespaceName";
import { OutcomeBadge, QuarantineTag, TestRunTag } from "../../components/Status";
import { formatCount, formatDuration, formatRelative, shortRunId } from "../../model/format";
import { fieldTerm, labelTerm, queryLink } from "../../model/query";
import type { RunModel } from "../../model/run";
import { isActive, type Run, type Waiting } from "../../model/types";
import { endedRunOf, workflowPath, workflowRef } from "../../model/workflows";
import { useAccess } from "../../state/access";
import { useRunExport } from "../../state/runs";
import { useWorkflowLabels } from "../../state/workflows";
import { RunControls } from "./RunControls";
import { endingOf, RunEndBanner } from "./RunEnd";

function ExportButton({ runId, as }: { runId: string; as: "run" | "cassette" }) {
  const download = useRunExport();
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState<string | null>(null);
  return (
    <button
      type="button"
      className="btn"
      disabled={busy}
      title={
        failed ??
        (as === "run"
          ? "The run export: its whole history, which replays on its own (rewind replay)"
          : "A cassette of its calls and signals, to test against (rewind test)")
      }
      onClick={async () => {
        setBusy(true);
        setFailed(null);
        try {
          const blob = await download(runId, as);
          const url = URL.createObjectURL(blob);
          const a = document.createElement("a");
          a.href = url;
          a.download = `${as}-${runId}.json`;
          a.click();
          setTimeout(() => URL.revokeObjectURL(url), 1_000);
        } catch (e) {
          setFailed(e instanceof Error ? e.message : String(e));
        } finally {
          setBusy(false);
        }
      }}
    >
      <Download size={14} aria-hidden="true" /> {as === "run" ? "Export run" : "Export cassette"}
    </button>
  );
}

/** The run's tags beside its handle, in its header, when they say something: a test run, a
 * quarantine. Not its status: its sections and banners say how it goes. */
export function RunTags({ run }: { run: Run }) {
  if (run.mode !== "test" && !run.quarantine) return null;
  return (
    <span className="run-tags">
      {run.mode === "test" && <TestRunTag />}
      {run.quarantine && <QuarantineTag />}
    </span>
  );
}

/**
 * The header's controls of the run's page: its forks, the Controls menu beside them, its export
 * and its cassette.
 */
export function RunHeaderControls({ run }: { run: Run }) {
  return (
    <>
      <Link
        className="btn"
        to={queryLink("/runs", fieldTerm("source", run.id))}
        title="Runs forked from this one"
      >
        <GitFork size={14} aria-hidden="true" /> Forks
      </Link>
      <RunControls run={run} />
      <ExportButton runId={run.id} as="run" />
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
  const labelOf = useWorkflowLabels();
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
            to={queryLink("/runs", fieldTerm("namespace", run.namespace))}
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
              to={workflowPath(run.workflow.id, run.workflow.version)}
              title={`The workflow ${run.workflow.id}, at the version the run executes`}
            >
              {workflowRef(run.workflow, labelOf(run.workflow.id))}
            </Link>
          </dd>
        </div>
      )}
      {run.startedBy && (
        <div>
          <dt>Started by</dt>
          <dd>
            <Link
              to={runPath(run.startedBy.handle)}
              title={`Started by the run ${run.startedBy.run}, at its call #${run.startedBy.seq}${
                run.startedBy.detached
                  ? ": detached, it lives on its own"
                  : ": that run does not end before this one, and cancels it if it fails"
              }`}
            >
              <RunHandle handle={run.startedBy.handle} copy={false} />
              {run.startedBy.detached && " · detached"}
            </Link>
          </dd>
        </div>
      )}
      {run.source && (
        <div>
          <dt>Fork of</dt>
          <dd>
            <Link
              to={runPath(run.source.handle)}
              title={`Forked from ${run.source.run}${run.source.at != null ? ` at event ${run.source.at}` : ""}`}
            >
              <RunHandle handle={run.source.handle} copy={false} />
            </Link>
          </dd>
        </div>
      )}
      <div>
        <dt>Code</dt>
        <dd>
          <Digest digest={run.codeDigest} />
        </dd>
      </div>
      <div>
        <dt>Action set</dt>
        <dd>
          <Link
            to={queryLink("/runs", fieldTerm("action_set", run.actionSet))}
            title="The runs of this action set"
          >
            <Digest digest={run.actionSet} />
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
        <dt>Events · epoch</dt>
        <dd className="num">
          {formatCount(run.historyLen)} · {run.epoch}
        </dd>
      </div>
      <div>
        <dt>Id</dt>
        <dd>
          <RunId id={run.id} />
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
      {Object.keys(run.secrets ?? {}).length > 0 && (
        <div>
          <dt>Secrets</dt>
          <dd>
            <UsedSecrets run={run} />
          </dd>
        </div>
      )}
      {Object.keys(run.labels).length > 0 && (
        <div className="run-details-labels">
          <dt>Labels</dt>
          <dd>
            <LabelList
              labels={run.labels}
              onPick={(pair) => {
                const at = pair.indexOf(":");
                void navigate(queryLink("/runs", labelTerm(pair.slice(0, at), pair.slice(at + 1))));
              }}
            />
          </dd>
        </div>
      )}
    </dl>
  );
}

/** The secrets the run uses, by server: names, never values; each opens where it is listed. */
function UsedSecrets({ run }: { run: Run }) {
  const access = useAccess();
  const reads = access.can("secrets:read");
  return (
    <ul className="used-secrets">
      {Object.entries(run.secrets ?? {})
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([server, ref]) => {
          const to = ref.startsWith("org/")
            ? "/secrets"
            : `/namespaces/${encodeURIComponent(run.namespace)}`;
          return (
            <li key={server}>
              <span className="muted">{server}</span>{" "}
              {reads ? (
                <Link className="mono" to={to} title="Where the secret is listed">
                  {ref}
                </Link>
              ) : (
                <span className="mono">{ref}</span>
              )}
            </li>
          );
        })}
    </ul>
  );
}

function waitingText(w: Waiting, now: number): string {
  switch (w.kind) {
    case "call": {
      const delivery = w.delivery > 1 ? `, delivery ${w.delivery}` : "";
      const state = {
        approval: "waits for the harness's approval",
        ready: "is ready for a worker",
        claimed: "is with a worker",
        deferred: "was deferred by its handler",
        parked: `is parked (${(w.parked ?? "parked").replaceAll("_", " ")})`,
        done: "is done",
      }[w.state];
      return `#${w.seq} ${w.action} ${state}${delivery}`;
    }
    case "timer":
      return `timer #${w.seq} fires ${formatRelative(Date.parse(w.fireAt), now)}`;
    case "signal": {
      const ended = endedRunOf(w.name);
      const what =
        ended === null ? `signal "${w.name}"` : `the end of run ${shortRunId(ended)} (result())`;
      return `${what}${w.timeoutAt ? `, times out ${formatRelative(Date.parse(w.timeoutAt), now)}` : ", no timeout"}`;
    }
    case "quarantined":
      return "a quarantine: an operator releases it, or gives it up";
  }
}

function waitingHint(w: Waiting): string | null {
  if (w.kind === "call" && w.state === "approval")
    return `Approve: POST /v1/calls/${w.callId}/approve · reject: …/reject`;
  if (w.kind === "call" && w.parked === "schema_changed")
    return `The action no longer matches its pinned schema: approve or reject call ${w.callId}, fork or cancel the run.`;
  if (w.kind === "call" && w.parked === "credentials_refused")
    return "The server refused the call: put the secret the run uses again, or approve the call once the server accepts the key.";
  if (w.kind === "call" && w.parked === "secret_missing")
    return "The secret the run uses no longer exists: put it again.";
  if (w.kind === "signal" && endedRunOf(w.name) !== null) return null;
  if (w.kind === "signal") return `Send it: POST /v1/runs/{id}/signals/${w.name}`;
  return null;
}

/** The activity a waiting item is about, to select it. */
function waitingIdx(w: Waiting, model: RunModel): number | null {
  if (w.kind === "call") return model.calls.find((c) => c.seq === w.seq)?.idx ?? null;
  if (w.kind === "timer") return model.timers.find((t) => t.seq === w.seq)?.idx ?? null;
  if (w.kind === "signal")
    return model.signals.find((s) => s.name === w.name && s.end === null)?.idx ?? null;
  return null;
}

export function RunBanners({
  run,
  model,
  onSelectEvent,
  onSelectLine,
}: {
  run: Run;
  model: RunModel;
  onSelectEvent: (idx: number) => void;
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
            ? "Its replay diverged from its history"
            : "Its steps kept failing"}
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
        onSelectEvent={onSelectEvent}
        onSelectLine={onSelectLine}
      />,
    );
  }

  const active = isActive(run.status);
  // Its quarantine has a banner of its own, above.
  const waiting = active ? (run.waitingFor ?? []).filter((w) => w.kind !== "quarantined") : [];
  if (waiting.length > 0 || (model.cancel && active)) {
    banners.push(
      <section key="w" className="banner tone-info-banner" aria-label="Waiting for">
        <h3>Waiting for</h3>
        <ul className="waiting-list">
          {waiting.map((w) => {
            const idx = waitingIdx(w, model);
            const hint = waitingHint(w);
            return (
              <li key={`${w.kind}-${w.kind === "signal" ? w.name : "seq" in w ? w.seq : ""}`}>
                <OutcomeBadge outcome="open" word="" />
                <button
                  type="button"
                  className="link-btn"
                  disabled={idx === null}
                  onClick={() => idx !== null && onSelectEvent(idx)}
                >
                  {waitingText(w, now)}
                </button>
                {w.callsite && <span className="muted mono">L{w.callsite.line}</span>}
                {hint && <span className="hint">{hint.replace("{id}", run.id)}</span>}
              </li>
            );
          })}
          {model.cancel && active && (
            <li>
              <OutcomeBadge outcome="open" word="" />
              Cancellation requested ({model.cancel.reason}): its code has one step to clean up.
            </li>
          )}
        </ul>
      </section>,
    );
  }

  return banners.length > 0 ? <div className="banners">{banners}</div> : null;
}
