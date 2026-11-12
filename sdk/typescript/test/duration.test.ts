import { describe, expect, it } from "vitest";

import { sliceUtf8, truncateEnd, truncateMiddle, utf8Length } from "../src/agent-tools/bound.js";
import { durationParam, formatDuration, toMillis } from "../src/duration.js";

describe("durations", () => {
  it("read as the server reads them", () => {
    expect(toMillis("500ms")).toBe(500);
    expect(toMillis("10s")).toBe(10_000);
    expect(toMillis("1h30m")).toBe(5_400_000);
    expect(toMillis(" 1h 30m 15s ")).toBe(5_415_000);
    expect(toMillis("1.5d")).toBe(129_600_000);
    expect(toMillis("2w")).toBe(1_209_600_000);
    expect(toMillis("0.5ms")).toBe(1);
    expect(toMillis("1500")).toBe(1500);
    expect(toMillis(1500.4)).toBe(1500);
  });

  it("refuse what the server refuses", () => {
    for (const bad of ["", "  ", "10 d", "3y", "-5s", "s", "1h-"]) {
      expect(() => toMillis(bad), bad).toThrow(RangeError);
    }
    expect(() => toMillis(-1)).toThrow(RangeError);
    expect(() => toMillis(Number.NaN)).toThrow(RangeError);
  });

  it("are written in their shortest form", () => {
    expect(formatDuration(0)).toBe("0ms");
    expect(formatDuration(500)).toBe("500ms");
    expect(formatDuration(5_400_000)).toBe("1h30m");
    expect(formatDuration(694_861_001)).toBe("1w1d1h1m1s1ms");
    expect(durationParam(30_000)).toBe("30s");
    expect(durationParam("90s")).toBe("1m30s");
  });
});

describe("bounds", () => {
  it("cut text on character boundaries", () => {
    const text = "aé€😀b";
    expect(utf8Length(text)).toBe(1 + 2 + 3 + 4 + 1);
    expect(sliceUtf8(text, 0, 2)).toEqual({ text: "a", start: 0, end: 1, total: 11 });
    expect(sliceUtf8(text, 2, 5)).toEqual({ text: "€", start: 3, end: 6, total: 11 });
    expect(sliceUtf8(text, 6, 100)).toEqual({ text: "😀b", start: 6, end: 11, total: 11 });
    expect(sliceUtf8(text, 50, 10)).toEqual({ text: "", start: 11, end: 11, total: 11 });
  });

  it("keep the start and the end of a long text", () => {
    const text = `${"head ".repeat(100)}${"x".repeat(10_000)}${" tail".repeat(100)}`;
    const cut = truncateMiddle(text, 2_000);
    expect(utf8Length(cut)).toBeLessThanOrEqual(2_000);
    expect(cut.startsWith("head head")).toBe(true);
    expect(cut.endsWith("tail tail")).toBe(true);
    expect(cut).toMatch(/\[… \d+ bytes left out …\]/);
    expect(truncateMiddle("short", 2_000)).toBe("short");
  });

  it("say how much they left out", () => {
    expect(truncateEnd("abcdef", 3)).toBe("abc[… 3 more bytes]");
    expect(truncateEnd("abc", 3)).toBe("abc");
  });
});
