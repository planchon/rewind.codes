// A payload to start from, made from its JSON Schema: what the form of a signal shows before the
// operator edits it. Only what the schema says is used: an example, a default, a constant, the
// first value of an enumeration, then a value of its type; an object holds its required
// properties, or all of them when it requires none.

type Schema = Record<string, unknown>;

function isSchema(value: unknown): value is Schema {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function typeOf(schema: Schema): string | null {
  const type = schema.type;
  if (typeof type === "string") return type;
  if (Array.isArray(type)) {
    const first = type.find((t) => t !== "null");
    return typeof first === "string" ? first : null;
  }
  if (isSchema(schema.properties)) return "object";
  if (schema.items !== undefined) return "array";
  return null;
}

export function exampleOf(schema: unknown, depth = 0): unknown {
  if (!isSchema(schema) || depth > 8) return null;
  if (Array.isArray(schema.examples) && schema.examples.length > 0) return schema.examples[0];
  if (schema.default !== undefined) return schema.default;
  if (schema.const !== undefined) return schema.const;
  if (Array.isArray(schema.enum) && schema.enum.length > 0) return schema.enum[0];
  for (const key of ["oneOf", "anyOf", "allOf"] as const) {
    const options = schema[key];
    if (Array.isArray(options) && options.length > 0) return exampleOf(options[0], depth + 1);
  }
  switch (typeOf(schema)) {
    case "object": {
      const properties = isSchema(schema.properties) ? schema.properties : {};
      const required = Array.isArray(schema.required)
        ? schema.required.filter((k): k is string => typeof k === "string")
        : [];
      const keys = required.length > 0 ? required : Object.keys(properties);
      const out: Record<string, unknown> = {};
      for (const key of keys) out[key] = exampleOf(properties[key], depth + 1);
      return out;
    }
    case "array":
      return [];
    case "string":
      return "";
    case "integer":
    case "number":
      return typeof schema.minimum === "number" ? schema.minimum : 0;
    case "boolean":
      return false;
    default:
      return null;
  }
}

/** The payload a form starts from, as the JSON it shows. */
export function examplePayload(schema: unknown): string {
  return JSON.stringify(exampleOf(schema), null, 2);
}
