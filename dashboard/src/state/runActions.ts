// What the run page does to a run (archi/05, "Run"): send it a signal, cancel it. Each with
// `runs:control`. A signal carries one `Idempotency-Key` per opening of its form, so that a retry
// within one opening delivers one signal; a cancellation needs none, the server takes a second one
// as the first. A signal shows "sent" until its `signal_received` entry arrives on the run's
// event stream; a cancellation, "cancelling" until the run's summary arrives cancelled.
import { useCallback, useEffect, useMemo, useSyncExternalStore } from "react";
import { isEnded, type Run, type Workflow } from "../model/types";
import { useRunSummary } from "./runs";
import { type Store, useStore } from "./store";
import { useWorkflowVersions } from "./workflows";

function newKey(): string {
  return (
    globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random().toString(16).slice(2)}`
  );
}

interface ControlSnapshot {
  signalPending: boolean;
  signalError: unknown;
  /** The signal sent, and the journal's length when it was sent. */
  sent: { name: string; from: number } | null;
  cancelPending: boolean;
  cancelError: unknown;
  cancelRequested: boolean;
}

/** What one run's controls hold, for every view of the run. */
class RunControl {
  private snapshot: ControlSnapshot = {
    signalPending: false,
    signalError: null,
    sent: null,
    cancelPending: false,
    cancelError: null,
    cancelRequested: false,
  };
  private readonly listeners = new Set<() => void>();
  private signalKey = newKey();

  constructor(
    private readonly store: Store,
    private readonly runId: string,
  ) {}

  getSnapshot = (): ControlSnapshot => this.snapshot;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  private set(change: Partial<ControlSnapshot>): void {
    this.snapshot = { ...this.snapshot, ...change };
    for (const listener of [...this.listeners]) listener();
  }

  openSignal(): void {
    this.signalKey = newKey();
    if (this.snapshot.signalError !== null) this.set({ signalError: null });
  }

  async sendSignal(name: string, payload: unknown): Promise<boolean> {
    if (this.snapshot.signalPending) return false;
    const from = this.store.journal(this.runId).getSnapshot().entries.length;
    this.set({ signalPending: true, signalError: null });
    try {
      await this.store.api.signalRun(this.runId, name, payload, this.signalKey);
      this.set({ signalPending: false, sent: { name, from } });
      return true;
    } catch (error) {
      this.set({ signalPending: false, signalError: error });
      return false;
    }
  }

  openCancel(): void {
    if (this.snapshot.cancelError !== null) this.set({ cancelError: null });
  }

  async cancel(): Promise<boolean> {
    if (this.snapshot.cancelPending) return false;
    this.set({ cancelPending: true, cancelError: null });
    try {
      await this.store.api.cancelRun(this.runId);
      this.set({ cancelPending: false, cancelRequested: true });
      // The run's details say it as soon as the run has taken the request in.
      this.store.runTouched(this.runId);
      return true;
    } catch (error) {
      this.set({ cancelPending: false, cancelError: error });
      return false;
    }
  }
}

const controls = new WeakMap<Store, Map<string, RunControl>>();

function controlOf(store: Store, runId: string): RunControl {
  let byRun = controls.get(store);
  if (!byRun) {
    byRun = new Map();
    controls.set(store, byRun);
  }
  let control = byRun.get(runId);
  if (!control) {
    control = new RunControl(store, runId);
    byRun.set(runId, control);
  }
  return control;
}

function useControl(runId: string): [RunControl, ControlSnapshot] {
  const store = useStore();
  const control = controlOf(store, runId);
  return [
    control,
    useSyncExternalStore(control.subscribe, control.getSnapshot, control.getSnapshot),
  ];
}

export interface SignalRun {
  /** The form opens: a new `Idempotency-Key`, kept for every send until it opens again. */
  open: () => void;
  /** Sends the signal; answers whether the server took it (202). */
  send: (name: string, payload: unknown) => Promise<boolean>;
  pending: boolean;
  /** The server's refusal (422: undeclared, or a payload its schema refuses; 409: ended). */
  error: unknown;
  /** The signal sent whose `signal_received` has not arrived yet. */
  sent: string | null;
}

export function useSignalRun(runId: string): SignalRun {
  const store = useStore();
  const [control, s] = useControl(runId);
  const journal = store.journal(runId);
  useEffect(() => journal.retain(), [journal]);
  const entries = useSyncExternalStore(
    journal.subscribe,
    () => journal.getSnapshot().entries,
    () => journal.getSnapshot().entries,
  );
  const sent = s.sent;
  const arrived =
    sent !== null &&
    entries
      .slice(sent.from)
      .some((e) => e.event.type === "signal_received" && e.event.name === sent.name);
  return {
    open: useCallback(() => control.openSignal(), [control]),
    send: useCallback(
      (name: string, payload: unknown) => control.sendSignal(name, payload),
      [control],
    ),
    pending: s.signalPending,
    error: s.signalError,
    sent: sent !== null && !arrived ? sent.name : null,
  };
}

export interface CancelRun {
  /** The confirmation opens. */
  open: () => void;
  /** Asks for the cancellation; answers whether the server took it (202). */
  cancel: () => Promise<boolean>;
  pending: boolean;
  error: unknown;
  /** Asked, and the run has not ended yet. */
  cancelling: boolean;
}

export function useCancelRun(run: Run): CancelRun {
  const [control, s] = useControl(run.id);
  const summary = useRunSummary(run.id);
  const ended = isEnded(run.status) || (summary !== undefined && isEnded(summary.status));
  return {
    open: useCallback(() => control.openCancel(), [control]),
    cancel: useCallback(() => control.cancel(), [control]),
    pending: s.cancelPending,
    error: s.cancelError,
    cancelling: s.cancelRequested && !ended,
  };
}

export interface DeclaredSignal {
  name: string;
  /** The JSON Schema of its payload. */
  schema: unknown;
}

/** The signals a run's workflow declares, at the run's version; none for a run of a script. */
export function useWorkflowSignals(run: Run): { signals: DeclaredSignal[]; loading: boolean } {
  const workflow = run.workflow ?? null;
  const versions = useWorkflowVersions(workflow?.name ?? null);
  const data: Workflow[] | undefined = versions.data;
  const signals = useMemo(() => {
    if (!workflow || !data) return [];
    const version = data.find((v) => v.version === workflow.version) ?? data[0];
    return Object.entries(version?.signals ?? {})
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([name, schema]) => ({ name, schema }));
  }, [workflow, data]);
  return { signals, loading: workflow !== null && versions.isPending };
}
