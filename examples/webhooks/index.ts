// Lifecycle webhooks: subscribe, verify each delivery, and act on the events.
//
//   WEBHOOK_URL=https://<your tunnel>/rewind-events pnpm webhooks
//
// Rewind delivers to public https:// URLs. In development, expose this process with a tunnel,
// or run a server of your own with --allow-private-webhooks to deliver to http://127.0.0.1.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { Rewind, WebhookVerificationFailed, verifyWebhook, type WebhookEvent } from "@rewind.codes/sdk";

const rewind = new Rewind();
const port = Number(process.env.PORT ?? 3000);
const url = process.env.WEBHOOK_URL ?? `http://127.0.0.1:${port}/rewind-events`;

// 1. Subscribe. The secret is in this answer only: a real harness stores it.
const subscription = await rewind.subscriptions.create({
  url,
  events: ["run.completed", "run.failed", "run.suspended", "quota.alert"],
  labels: { example: "webhooks" }, // only runs carrying these labels
});
const secret = subscription.secret ?? process.env.REWIND_WEBHOOK_SECRET ?? "";

let completed: () => void;
const done = new Promise<void>((resolve) => (completed = resolve));

function handle(event: WebhookEvent): void {
  switch (event.type) {
    case "run.suspended":
      // The run waits for the outside world: a signal, a timer, an approval.
      console.log(`suspended ${event.data.run.id}, waiting for`, event.data.run.waitingFor?.map((w) => w.kind));
      break;
    case "run.completed":
      // Resume the conversation that started the run: its output is here.
      console.log(`completed ${event.data.run.id} for ${event.data.run.namespace}:`, event.data.run.output?.value);
      completed();
      break;
    case "run.failed":
      console.log(`failed ${event.data.run.id}:`, event.data.run.error?.message);
      break;
    case "quota.alert":
      console.log(`${event.data.namespace ?? "the organization"} crossed ${event.data.threshold} % of ${event.data.quota}`);
      break;
  }
}

// 2. Receive. Verify the body exactly as received, answer fast, and deduplicate on webhook-id:
//    a retried delivery keeps it.
const seen = new Set<string>();
const server = createServer(async (req, res) => {
  if (req.method !== "POST" || req.url !== "/rewind-events") return void res.writeHead(404).end();
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);

  let event: WebhookEvent;
  try {
    event = verifyWebhook(Buffer.concat(chunks), req.headers, secret);
  } catch (error) {
    if (error instanceof WebhookVerificationFailed) return void res.writeHead(400).end();
    throw error;
  }
  res.writeHead(204).end();

  const id = String(req.headers["webhook-id"]);
  if (seen.has(id)) return;
  seen.add(id);
  handle(event);
});
server.listen(port);

// 3. A run that suspends on a signal, then completes once it gets it.
const user = rewind.namespace("user_ada");
const run = await user.runs.start({
  script: await readFile(new URL("./scripts/greet.ts", import.meta.url), "utf8"),
  labels: { example: "webhooks" },
});
setTimeout(() => void user.runs.signal(run.id, "hello", { name: "Ada" }), 2_000);

await done;
server.close();
await rewind.subscriptions.delete(subscription.id);
