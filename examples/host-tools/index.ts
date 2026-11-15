// Host tools: functions of the harness that scripts call, executed by the SDK's worker.
//
//   pnpm host-tools
import { readFile } from "node:fs/promises";
import { HostToolError, Rewind } from "@rewind.codes/sdk";

const rewind = new Rewind();
const object = (properties: Record<string, unknown>, required = Object.keys(properties)) => ({
  type: "object",
  properties,
  required,
});

const toolset = await rewind.toolsets.create({
  host: [
    {
      name: "crm.find_customer",
      description: "A customer by email. Fails with code not_found when there is none.",
      inputSchema: object({ email: { type: "string" } }),
      outputSchema: object({ id: { type: "string" }, since: { type: "string" } }),
    },
    {
      name: "crm.invite",
      description: "Invite someone to the beta.",
      inputSchema: object({ email: { type: "string" } }),
      outputSchema: object({ id: { type: "string" } }),
    },
    {
      name: "ask_user",
      description: "Ask the end user a question and return their answer. May take days.",
      inputSchema: object({ question: { type: "string" } }),
      outputSchema: object({ answer: { type: "string" } }),
      // A question must not be asked twice if the worker dies while asking it.
      policy: { onInterruption: "at_most_once" },
    },
  ],
});

// Where the harness keeps questions it has asked: in a real product, a table of its database.
const inbox = new Map<string, { question: string; token: string }>();

const worker = rewind.worker({
  toolset: toolset.id,
  concurrency: 10,
  tools: {
    "crm.find_customer": async ({ email }: { email: string }, ctx) => {
      // ctx.namespace is the end user the run acts for: read their data only.
      console.log(`[crm] find ${email} for ${ctx.namespace}, attempt ${ctx.attempt}`);
      throw new HostToolError(`no customer ${email}`, { data: { code: "not_found" } });
    },
    "crm.invite": async ({ email }: { email: string }, ctx) => {
      // The key is the same on every attempt of this call: give it to your API, and a call made
      // again after an interruption does not invite twice.
      console.log(`[crm] invite ${email}, idempotency key ${ctx.idempotencyKey}`);
      return { id: `inv_${email.split("@")[0]}` };
    },
    ask_user: async ({ question }: { question: string }, ctx) => {
      // Answer later: the task is deferred, needs no heartbeat, and is answered by its token,
      // from any process, possibly days from now.
      const later = await ctx.defer({ timeout: "3d" });
      inbox.set(ctx.runId, { question, token: later.token });
      console.log(`[ask_user] "${question}" sent to ${ctx.namespace}`);
    },
  },
});

const user = rewind.namespace("user_ada");
const script = await readFile(new URL("./scripts/invite.ts", import.meta.url), "utf8");
let run = await user.runs.start({
  toolset: toolset.id,
  script,
  input: { email: "grace@example.net" },
  wait: "5s",
  labels: { example: "host-tools" },
});
console.log(`run ${run.id}: ${run.status}, waiting for`, run.waitingFor?.map((w) => w.kind));

// Later, when the user answers in your product, whoever holds the token answers the task.
const asked = inbox.get(run.id);
if (asked !== undefined) await rewind.tasks.complete(asked.token, { answer: "yes" });

run = await user.runs.get(run.id, { wait: "10s" });
console.log(`run ${run.id}: ${run.status}`, run.output?.value);
await worker.stop();
