import { describe, expect, it } from "vitest";
import {
  fitLabels,
  formatBytes,
  formatDuration,
  formatOffset,
  formatPercent,
  formatRelative,
  formatTime,
  shortDigest,
  shortRunId,
  splitLabel,
} from "./format";

describe("format", () => {
  it("formats durations from microseconds to days", () => {
    expect(formatDuration(0)).toBe("0ms");
    expect(formatDuration(0.84)).toBe("840µs");
    expect(formatDuration(310)).toBe("310ms");
    expect(formatDuration(1_240)).toBe("1.24s");
    expect(formatDuration(2_000)).toBe("2s");
    expect(formatDuration(12_340)).toBe("12.3s");
    expect(formatDuration(245_000)).toBe("4m 05s");
    expect(formatDuration(3 * 3_600_000 + 7 * 60_000)).toBe("3h 07m");
    expect(formatDuration(3 * 86_400_000)).toBe("3d");
    expect(formatDuration(3 * 86_400_000 + 4 * 3_600_000)).toBe("3d 4h");
    expect(formatOffset(0)).toBe("+0s");
    expect(formatOffset(300)).toBe("+300ms");
  });

  it("formats sizes in powers of 1024", () => {
    expect(formatBytes(812)).toBe("812 B");
    expect(formatBytes(12_697)).toBe("12 KB");
    expect(formatBytes(1_536)).toBe("1.5 KB");
    expect(formatBytes(3 * 1024 * 1024)).toBe("3.0 MB");
  });

  it("formats ratios", () => {
    expect(formatPercent(0, 10)).toBe("0%");
    expect(formatPercent(1, 3)).toBe("33%");
    expect(formatPercent(1, 250)).toBe("0.4%");
    expect(formatPercent(1, 10_000)).toBe("<0.1%");
    expect(formatPercent(1, 0)).toBe("–");
  });

  it("formats times in UTC", () => {
    const t = Date.parse("2026-11-09T10:02:11.042Z");
    expect(formatTime(t, "utc")).toBe("2026-11-09 10:02:11");
    expect(formatTime(t, "utc", true)).toBe("2026-11-09 10:02:11.042");
  });

  it("formats relative times", () => {
    const now = Date.parse("2026-11-12T10:00:00Z");
    expect(formatRelative(now - 2_000, now)).toBe("just now");
    expect(formatRelative(now - 42_000, now)).toBe("42s ago");
    expect(formatRelative(now - 3 * 3_600_000, now)).toBe("3h ago");
    expect(formatRelative(now + 2 * 86_400_000, now)).toBe("in 2d");
  });

  it("shortens ids and digests, and splits labels", () => {
    expect(shortDigest("9f2c4e1a07b3c9d2e8f1")).toBe("9f2c4e1a07b3");
    expect(shortRunId("01932f5e-7c1a-7b3e-9f2c-4e1a07b3c9d2")).toBe("01932f5e…b3c9d2");
    expect(splitLabel("conversation:c-42:x")).toEqual(["conversation", "c-42:x"]);
    expect(splitLabel("nocolon")).toBeNull();
  });

  it("fits whole chips in a cell, their values never cut", () => {
    const labels: [string, string][] = [
      ["dax", "iteration"],
      ["env", "development"],
      ["goal", "6afc64fdf4d56d8262048836"],
      ["iteration", "6afc64fe1c0b5e2a9d7f3a41"],
      ["kind", "routine"],
      ["routine", "notifications_daily"],
    ];
    expect(fitLabels(labels)).toEqual({ shown: labels.slice(0, 2), hidden: 4 });
    // One label is always shown, however long.
    expect(fitLabels([["a-very-long-label-key-indeed", "6afc64fdf4d56d8262048836"]], 10)).toEqual({
      shown: [["a-very-long-label-key-indeed", "6afc64fdf4d56d8262048836"]],
      hidden: 0,
    });
  });
});
