import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { createWorld } from "../mocks/fixtures";
import { createHandlers } from "../mocks/handlers";
import { JournalBuilder } from "../mocks/journal";
import type { Entry, RunStatus, StreamEvent } from "../model/types";
import { server } from "../test/server";
import { Api, ApiError } from "./client";
import { followRun } from "./runEvents";

const NOW = Date.parse("2026-11-12T10:00:00Z");
const api = new Api({ baseUrl: window.location.origin, token: () => "rewind_sk_k" });

function frame(event: StreamEvent): string {
  const id = event.type === "entry" ? `id: ${event.idx}\n` : "";
  return `event: ${event.type}\n${id}data: ${JSON.stringify(event)}\n\n`;
}

function collect() {
  const entries: Entry[] = [];
  const ends: RunStatus[] = [];
  return {
    entries,
    ends,
    handlers: {
      onEntries: (batch: Entry[]) => entries.push(...batch),
      onEnd: (status: RunStatus) => ends.push(status),
    },
  };
}

describe("followRun", () => {
  it("reads every entry of an ended run, then its end", async () => {
    const world = createWorld({ now: NOW });
    server.use(...createHandlers(world, { now: () => NOW }));
    const run = world.runs[0];
    const out = collect();
    await followRun(api, run?.id ?? "", out.handlers, new AbortController().signal);
    expect(out.entries.map((e) => e.idx)).toEqual(run?.journal.entries.map((e) => e.idx));
    expect(out.ends).toEqual(["completed"]);
  });

  it("sends the key, and asks to follow from the first entry", async () => {
    let url = "";
    let auth: string | null = null;
    server.use(
      http.get("*/v1/runs/:id/events", ({ request }) => {
        url = request.url;
        auth = request.headers.get("Authorization");
        return new HttpResponse(frame({ type: "end", status: "completed" }), {
          headers: { "Content-Type": "text/event-stream" },
        });
      }),
    );
    await followRun(api, "r1", collect().handlers, new AbortController().signal);
    expect(new URL(url).searchParams.get("follow")).toBe("1");
    expect(new URL(url).searchParams.get("from")).toBe("0");
    expect(auth).toBe("Bearer rewind_sk_k");
  });

  it("resumes after a dropped stream from the next entry, without duplicates", async () => {
    const j = new JournalBuilder(NOW)
      .started({ script: "s", toolset: "t" })
      .call(1, "a.b", {}, 3)
      .ok(1, 1);
    const [e0, e1, e2] = j.entries;
    const froms: string[] = [];
    let attempt = 0;
    server.use(
      http.get("*/v1/runs/:id/events", ({ request }) => {
        froms.push(new URL(request.url).searchParams.get("from") ?? "");
        attempt += 1;
        // The first stream drops after two entries; the second one sends the second again.
        const body =
          attempt === 1
            ? frame({ type: "entry", ...(e0 as Entry) }) +
              frame({ type: "entry", ...(e1 as Entry) })
            : frame({ type: "entry", ...(e1 as Entry) }) +
              frame({ type: "entry", ...(e2 as Entry) }) +
              frame({ type: "end", status: "active" });
        return new HttpResponse(body, { headers: { "Content-Type": "text/event-stream" } });
      }),
    );
    const out = collect();
    await followRun(api, "r1", out.handlers, new AbortController().signal, { backoffMs: [1] });
    expect(froms).toEqual(["0", "2"]);
    expect(out.entries.map((e) => e.idx)).toEqual([0, 1, 2]);
    expect(out.ends).toEqual(["active"]);
  });

  it("resumes with a fresh token when the server closes the stream at the token's expiry", async () => {
    const j = new JournalBuilder(NOW)
      .started({ script: "s", toolset: "t" })
      .call(1, "a.b", {}, 3)
      .ok(1, 1);
    const [e0, e1, e2] = j.entries;
    const seen: { from: string; auth: string | null }[] = [];
    let token = "at_1";
    server.use(
      http.get("*/v1/runs/:id/events", ({ request }) => {
        seen.push({
          from: new URL(request.url).searchParams.get("from") ?? "",
          auth: request.headers.get("Authorization"),
        });
        if (seen.length === 1) {
          // The session refreshed the token meanwhile; the server closes this stream when the
          // token it was opened with expires.
          token = "at_2";
          return new HttpResponse(
            frame({ type: "entry", ...(e0 as Entry) }) + frame({ type: "entry", ...(e1 as Entry) }),
            { headers: { "Content-Type": "text/event-stream" } },
          );
        }
        return new HttpResponse(
          frame({ type: "entry", ...(e2 as Entry) }) + frame({ type: "end", status: "completed" }),
          { headers: { "Content-Type": "text/event-stream" } },
        );
      }),
    );
    const member = new Api({ baseUrl: window.location.origin, token: () => token });
    const out = collect();
    await followRun(member, "r1", out.handlers, new AbortController().signal, { backoffMs: [1] });
    expect(seen).toEqual([
      { from: "0", auth: "Bearer at_1" },
      { from: "2", auth: "Bearer at_2" },
    ]);
    expect(out.entries.map((e) => e.idx)).toEqual([0, 1, 2]);
    expect(out.ends).toEqual(["completed"]);
  });

  it("retries after a server error, and stops on an unknown run", async () => {
    let calls = 0;
    server.use(
      http.get("*/v1/runs/:id/events", () => {
        calls += 1;
        if (calls === 1) return new HttpResponse("upstream down", { status: 503 });
        return HttpResponse.json(
          {
            type: "urn:rewind:problem:not-found",
            title: "Not found",
            status: 404,
            detail: "No run r9.",
          },
          { status: 404 },
        );
      }),
    );
    const states: string[] = [];
    const error = await followRun(
      api,
      "r9",
      { ...collect().handlers, onState: (s) => states.push(s) },
      new AbortController().signal,
      { backoffMs: [1] },
    ).catch((e: unknown) => e);
    expect(calls).toBe(2);
    expect(states).toContain("retrying");
    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).slug).toBe("not-found");
  });

  it("stops when aborted, while the run goes on", async () => {
    const world = createWorld({ now: NOW });
    // An active run: the stream stays open.
    const active = world.runs[1];
    server.use(...createHandlers(world, { now: () => NOW, pollMs: 5 }));
    const controller = new AbortController();
    const out = collect();
    const done = followRun(
      api,
      active?.id ?? "",
      {
        ...out.handlers,
        onEntries: (batch) => {
          out.entries.push(...batch);
          controller.abort();
        },
      },
      controller.signal,
    );
    await done;
    expect(out.entries.length).toBe(active?.journal.entries.length);
    expect(out.ends).toEqual([]);
  });
});
