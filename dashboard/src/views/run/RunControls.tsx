// The run's controls, in its sidebar, for whoever holds `runs:control` (archi/05, "Run"): send it
// a signal ("Send approval" when it waits for one, "Send a signal" for any name), cancel it while
// it has not ended. A signal is written in a dialog, its payload as JSON: for a run of a workflow,
// the signals its version declares, each payload started from its schema; the server's refusal
// shows under the payload. A signal shows "sent" until its `signal_received` arrives on the run's
// event stream; a cancellation, "Cancelling" until the run's summary arrives cancelled.
import { Ban, Send } from "lucide-react";
import { useEffect, useId, useMemo, useState } from "react";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { DialogForm, FormDialog } from "../../components/FormDialog";
import { shortRunId } from "../../model/format";
import { examplePayload } from "../../model/schema";
import { isEnded, type Run } from "../../model/types";
import { endedRunOf } from "../../model/workflows";
import { useAccess } from "../../state/access";
import {
  type DeclaredSignal,
  useCancelRun,
  useSignalRun,
  useWorkflowSignals,
} from "../../state/runActions";

/** The signals the run waits for now, those its script awaits (not the end of a run it started). */
function awaitedSignals(run: Run): string[] {
  if (run.status !== "active") return [];
  const names = (run.waitingFor ?? []).flatMap((w) =>
    w.kind === "signal" && endedRunOf(w.name) === null ? [w.name] : [],
  );
  return [...new Set(names)];
}

class PayloadError extends Error {
  override readonly name = "PayloadError";
}

function SignalForm({
  run,
  preset,
  declared,
  onSent,
}: {
  run: Run;
  /** The signal the form opened for; `null` for any. */
  preset: string | null;
  declared: readonly DeclaredSignal[];
  onSent: () => void;
}) {
  const signal = useSignalRun(run.id);
  const nameId = useId();
  const { open } = signal;
  // One Idempotency-Key per opening of the form: a retry within it delivers one signal.
  useEffect(() => open(), [open]);
  const [name, setName] = useState(preset ?? declared[0]?.name ?? "");
  const schemaOf = (n: string) => declared.find((d) => d.name === n)?.schema;
  const startFor = (n: string) => {
    const schema = schemaOf(n);
    return schema === undefined ? "{}" : examplePayload(schema);
  };
  const [payload, setPayload] = useState(() => startFor(name));
  const [edited, setEdited] = useState(false);
  const [parseError, setParseError] = useState<PayloadError | null>(null);
  const schema = schemaOf(name);
  const pick = (next: string) => {
    setName(next);
    if (!edited) setPayload(startFor(next));
  };
  const submit = async () => {
    let value: unknown;
    try {
      value = payload.trim() === "" ? null : JSON.parse(payload);
    } catch (e) {
      setParseError(
        new PayloadError(`The payload is not JSON: ${e instanceof Error ? e.message : String(e)}`),
      );
      return;
    }
    setParseError(null);
    if (await signal.send(name.trim(), value)) onSent();
  };
  return (
    <DialogForm
      title={preset ? `Send ${preset}` : "Send a signal"}
      meta={<span className="mono">{shortRunId(run.id)}</span>}
      description={
        run.workflow
          ? `The run of ${run.workflow.name} receives it in its mailbox; the workflow declares the signals it takes and their payloads.`
          : "The run receives it in its mailbox, now or the next time its script waits for that name."
      }
      submit={
        <>
          <Send size={13} aria-hidden="true" /> Send
        </>
      }
      pending={signal.pending}
      pendingLabel="Sending…"
      disabled={name.trim() === ""}
      error={parseError ?? signal.error}
      errorTitle={parseError ? "The payload cannot be read" : undefined}
      onSubmit={() => void submit()}
    >
      <div className="field col">
        <label className="field-label" htmlFor={nameId}>
          Signal
        </label>
        {declared.length > 0 ? (
          <select
            id={nameId}
            className="input mono"
            value={name}
            onChange={(e) => pick(e.target.value)}
            aria-label="Signal"
          >
            {declared.map((d) => (
              <option key={d.name} value={d.name}>
                {d.name}
              </option>
            ))}
            {name !== "" && !declared.some((d) => d.name === name) && (
              <option value={name}>{name}</option>
            )}
          </select>
        ) : (
          <input
            id={nameId}
            className="input mono"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="approval"
            spellCheck={false}
            autoComplete="off"
            aria-label="Signal"
            readOnly={preset !== null}
          />
        )}
      </div>
      {schema !== undefined && (
        <details className="signal-schema">
          <summary>Its payload's schema</summary>
          <pre className="mono">{JSON.stringify(schema, null, 2)}</pre>
        </details>
      )}
      <label className="field col">
        <span className="field-label">Payload</span>
        <textarea
          className="input mono json-editor"
          value={payload}
          rows={8}
          spellCheck={false}
          aria-label="Payload"
          onChange={(e) => {
            setPayload(e.target.value);
            setEdited(true);
            if (parseError) setParseError(null);
          }}
        />
      </label>
    </DialogForm>
  );
}

function CancelControl({ run }: { run: Run }) {
  const cancel = useCancelRun(run);
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        type="button"
        className="btn danger-ghost control-btn"
        disabled={cancel.cancelling}
        onClick={() => {
          cancel.open();
          setOpen(true);
        }}
      >
        <Ban size={13} aria-hidden="true" /> {cancel.cancelling ? "Cancelling…" : "Cancel run"}
      </button>
      <ConfirmDialog
        open={open}
        onOpenChange={setOpen}
        title={`Cancel the run ${shortRunId(run.id)}?`}
        description={
          <>
            Every pending await of its script rejects with <code>CancelledError</code>, and the
            script gets one activation to clean up: the run ends cancelled.
          </>
        }
        confirm="Cancel run"
        pendingLabel="Cancelling…"
        pending={cancel.pending}
        error={cancel.error}
        onConfirm={() => {
          void cancel.cancel().then((done) => {
            if (done) setOpen(false);
          });
        }}
      />
    </>
  );
}

/** The run's controls, or nothing without `runs:control` or once the run has ended. */
export function RunControls({ run }: { run: Run }) {
  const access = useAccess();
  if (!access.can("runs:control") || isEnded(run.status)) return null;
  return <Controls run={run} />;
}

function Controls({ run }: { run: Run }) {
  const signal = useSignalRun(run.id);
  const { signals: declared } = useWorkflowSignals(run);
  const awaited = useMemo(() => awaitedSignals(run), [run]);
  const [form, setForm] = useState<{ preset: string | null } | null>(null);
  return (
    <section className="pane-section run-controls" aria-label="Controls">
      <h2 className="pane-label">Controls</h2>
      <div className="control-list">
        {awaited.map((name) => (
          <button
            key={name}
            type="button"
            className="btn primary control-btn"
            onClick={() => setForm({ preset: name })}
          >
            <Send size={13} aria-hidden="true" /> Send {name}
          </button>
        ))}
        <button type="button" className="btn control-btn" onClick={() => setForm({ preset: null })}>
          <Send size={13} aria-hidden="true" /> Send a signal
        </button>
        <CancelControl run={run} />
      </div>
      <p className="control-note muted" role="status">
        {signal.sent !== null ? `Signal ${signal.sent} sent` : ""}
      </p>
      <FormDialog open={form !== null} onOpenChange={(o) => !o && setForm(null)}>
        {form && (
          <SignalForm
            run={run}
            preset={form.preset}
            declared={declared}
            onSent={() => setForm(null)}
          />
        )}
      </FormDialog>
    </section>
  );
}
