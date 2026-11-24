import { describe, expect, it } from "vitest";
import { statsScope, waitingRuns } from "./scope";

describe("the runs waiting now", () => {
  it("adds the waits of every kind", () => {
    expect(
      waitingRuns({
        signal: 2,
        timer: 1,
        harness: 3,
        approval: 1,
        deferred: 1,
        parked: 0,
        quarantined: 1,
      }),
    ).toBe(9);
  });
});

describe("the scope of the statistics", () => {
  it("is the tenant without a namespace term", () => {
    expect(statsScope(undefined)).toEqual({ q: undefined, offset: 0, narrowed: false });
    expect(statsScope("")).toEqual({ q: undefined, offset: 0, narrowed: false });
    expect(statsScope("@agent:triage status:failed")).toEqual({
      q: undefined,
      offset: 0,
      narrowed: true,
    });
  });

  it("keeps one namespace term of one value, and says what it leaves out", () => {
    expect(statsScope("namespace:alice")).toEqual({
      q: "namespace:alice",
      offset: 0,
      narrowed: false,
    });
    expect(statsScope("@agent:triage namespace:alice status:failed")).toEqual({
      q: "namespace:alice",
      offset: 14,
      narrowed: true,
    });
    expect(statsScope('namespace:"a b"').q).toBe('namespace:"a b"');
  });

  it("counts the offset in characters", () => {
    expect(statsScope('"🦀" namespace:bob').offset).toBe(4);
  });

  it("is the tenant for a namespace excluded, several, a pattern or two terms", () => {
    for (const q of [
      "-namespace:alice",
      "namespace:alice,bob",
      "namespace:ali*",
      "namespace:alice namespace:bob",
      "namespace:",
    ]) {
      expect(statsScope(q)).toEqual({ q: undefined, offset: 0, narrowed: true });
    }
  });
});
