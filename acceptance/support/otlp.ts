import { randomBytes } from "node:crypto";

/** One span as the collector wrote it, flattened with its resource and scope. */
export interface SpanRecord {
  traceId: string;
  spanId: string;
  /** Empty for a root span. */
  parentSpanId: string;
  name: string;
  scope: string;
  attributes: Record<string, unknown>;
  resource: Record<string, unknown>;
}

interface OtlpValue {
  stringValue?: string;
  intValue?: string | number;
  boolValue?: boolean;
  doubleValue?: number;
  arrayValue?: { values?: OtlpValue[] };
}

interface OtlpAttribute {
  key: string;
  value: OtlpValue;
}

interface OtlpExport {
  resourceSpans?: {
    resource?: { attributes?: OtlpAttribute[] };
    scopeSpans?: {
      scope?: { name?: string };
      spans?: {
        traceId: string;
        spanId: string;
        parentSpanId?: string;
        name: string;
        attributes?: OtlpAttribute[];
      }[];
    }[];
  }[];
}

function value(v: OtlpValue): unknown {
  if (v.stringValue !== undefined) return v.stringValue;
  if (v.intValue !== undefined) return Number(v.intValue);
  if (v.boolValue !== undefined) return v.boolValue;
  if (v.doubleValue !== undefined) return v.doubleValue;
  if (v.arrayValue !== undefined) return (v.arrayValue.values ?? []).map(value);
  return undefined;
}

function attributes(list: OtlpAttribute[] | undefined): Record<string, unknown> {
  return Object.fromEntries((list ?? []).map((a) => [a.key, value(a.value)]));
}

/** Every span of the collector's export requests (OTLP JSON, ids in hex). */
export function flattenSpans(exports: unknown[]): SpanRecord[] {
  const spans: SpanRecord[] = [];
  for (const line of exports as OtlpExport[]) {
    for (const resourceSpans of line.resourceSpans ?? []) {
      const resource = attributes(resourceSpans.resource?.attributes);
      for (const scopeSpans of resourceSpans.scopeSpans ?? []) {
        for (const span of scopeSpans.spans ?? []) {
          spans.push({
            traceId: span.traceId.toLowerCase(),
            spanId: span.spanId.toLowerCase(),
            parentSpanId: (span.parentSpanId ?? "").toLowerCase(),
            name: span.name,
            scope: scopeSpans.scope?.name ?? "",
            attributes: attributes(span.attributes),
            resource,
          });
        }
      }
    }
  }
  return spans;
}

/** The harness's own span: the LLM call that wrote the code, the parent of its run. */
export class HarnessSpan {
  readonly traceId = randomBytes(16).toString("hex");
  readonly spanId = randomBytes(8).toString("hex");
  readonly start = BigInt(Date.now()) * 1_000_000n;

  constructor(readonly name: string) {}

  /** The W3C `traceparent` a harness passes when it starts the run. */
  get traceparent(): string {
    return `00-${this.traceId}-${this.spanId}-01`;
  }

  /** Exports the span to the collector, over OTLP/HTTP with a JSON body, as a harness's SDK would. */
  async export(collectorHttp: string): Promise<void> {
    const end = BigInt(Date.now()) * 1_000_000n;
    const body = {
      resourceSpans: [
        {
          resource: { attributes: [{ key: "service.name", value: { stringValue: "acceptance-harness" } }] },
          scopeSpans: [
            {
              scope: { name: "acceptance-harness" },
              spans: [
                {
                  traceId: this.traceId,
                  spanId: this.spanId,
                  name: this.name,
                  kind: 3,
                  startTimeUnixNano: this.start.toString(),
                  endTimeUnixNano: end.toString(),
                  attributes: [{ key: "gen_ai.operation.name", value: { stringValue: "chat" } }],
                },
              ],
            },
          ],
        },
      ],
    };
    const response = await fetch(`${collectorHttp}/v1/traces`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!response.ok) throw new Error(`the collector refused the harness span: ${response.status}`);
  }
}

/** The ids of a W3C `traceparent`. */
export function parseTraceparent(traceparent: string): { traceId: string; parentId: string } {
  const [, traceId, parentId] = traceparent.split("-");
  if (traceId === undefined || parentId === undefined) throw new Error(`not a traceparent: ${traceparent}`);
  return { traceId, parentId };
}
