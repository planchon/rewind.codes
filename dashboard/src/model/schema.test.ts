import { describe, expect, it } from "vitest";
import { exampleOf, examplePayload } from "./schema";

describe("a payload made from its schema", () => {
  it("holds an object's required properties, each of its type", () => {
    expect(
      exampleOf({
        type: "object",
        required: ["close", "reason", "count"],
        properties: {
          close: { type: "boolean" },
          reason: { type: "string" },
          count: { type: "integer", minimum: 1 },
          note: { type: "string" },
        },
      }),
    ).toEqual({ close: false, reason: "", count: 1 });
  });

  it("prefers an example, a default, a constant or an enumeration", () => {
    expect(exampleOf({ type: "string", examples: ["yes"] })).toBe("yes");
    expect(exampleOf({ type: "number", default: 3 })).toBe(3);
    expect(exampleOf({ const: "approve" })).toBe("approve");
    expect(exampleOf({ enum: ["approve", "reject"] })).toBe("approve");
    expect(exampleOf({ anyOf: [{ type: "array" }, { type: "null" }] })).toEqual([]);
  });

  it("is null without a schema, and written as JSON", () => {
    expect(exampleOf(true)).toBeNull();
    expect(examplePayload({ type: "object", properties: { a: { type: "string" } } })).toBe(
      '{\n  "a": ""\n}',
    );
  });
});
