import { describe, expect, it } from "vitest";
import { createWorld, SCRIPTS } from "../mocks/fixtures";
import { cancelCause, causeText, endKindOf, endTitle, pendingBefore } from "./ending";
import { buildRunModel } from "./run";
import { buildSteps } from "./steps";

const NOW = Date.parse("2026-11-18T12:00:00Z");
const world = createWorld({ now: NOW });

function journalOf(script: typeof SCRIPTS.DEV_NOTIFICATIONS) {
  const run = world.runs.find((r) => r.script === script);
  if (!run) throw new Error("no such fixture");
  return run.journal.entries;
}

describe("how a run ended", () => {
  it("tells a cancellation at the deadline from the journal, with what the run awaited", () => {
    const entries = journalOf(SCRIPTS.DEV_NOTIFICATIONS);
    const model = buildRunModel(entries);
    const steps = buildSteps(entries);
    // cancel_requested { ttl } then fail_run(CancelledError): the step ends the run cancelled.
    expect(steps.map((s) => s.ends)).toEqual([null, "cancelled"]);

    const start = model.start?.event.expiresAt as string;
    const cause = cancelCause({
      cancel: model.cancel,
      journalRead: true,
      createdAt: entries[0]?.recordedAt ?? null,
      expiresAt: start,
    });
    expect(cause).toEqual({ kind: "ttl", ttlMs: (11 * 60 + 29) * 60_000 });
    expect(endTitle("cancelled", cause)).toBe("Cancelled at its deadline");
    expect(causeText(cause)).toBe("Its deadline passed: a TTL of 11h 29m from its start.");

    const pending = pendingBefore(model, model.cancel?.idx as number);
    expect(pending.map((p) => [p.text, p.what, p.line])).toEqual([
      ['it was awaiting signal "notify"', 'signal "notify"', 25],
    ]);
  });

  it("finds the deadline from a run's details alone, and says nothing it cannot know", () => {
    const createdAt = "2026-11-17T10:00:00Z";
    const expiresAt = "2026-11-17T21:29:00Z";
    expect(cancelCause({ createdAt, expiresAt, endedAt: "2026-11-17T21:29:00Z" })).toEqual({
      kind: "ttl",
      ttlMs: (11 * 60 + 29) * 60_000,
    });
    const early = cancelCause({ createdAt, expiresAt, endedAt: "2026-11-17T12:00:00Z" });
    expect(early).toEqual({ kind: "unknown" });
    expect(endTitle("cancelled", early)).toBe("Cancelled");
    expect(causeText(early)).toBeNull();
  });

  it("names a request, and a CancelledError the script threw itself", () => {
    const requested = cancelCause({
      cancel: { reason: "requested", message: "The conversation was closed." },
      journalRead: true,
    });
    expect(endTitle("cancelled", requested)).toBe("Cancelled on request");
    expect(causeText(requested)).toBe(
      "Cancellation was requested: “The conversation was closed.”.",
    );
    const script = cancelCause({ cancel: null, journalRead: true });
    expect(endTitle("cancelled", script)).toBe("Cancelled by its script");
  });

  it("keeps failures and the other statuses as they are", () => {
    expect(endKindOf("failed")).toBe("failed");
    expect(endKindOf("active")).toBeNull();
    expect(endTitle("failed")).toBe("Failed");
    expect(endTitle("aborted")).toBe("Aborted by the host");
    // The triage run a failed call ended: its last step ends failed, not cancelled.
    const failed = world.runs.find(
      (r) => r.script === SCRIPTS.TRIAGE && r.labels.conversation === "c-39",
    );
    const steps = buildSteps(failed?.journal.entries ?? []);
    expect(steps[steps.length - 1]?.ends).toBe("failed");
  });
});
