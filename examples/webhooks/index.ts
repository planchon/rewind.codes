// Webhooks: subscribe, verify each delivery, and act on what it says.
//
//   WEBHOOK_URL=https://<your tunnel>/rewind-webhooks pnpm webhooks
//
// Rewind posts to public https:// URLs. In development, expose this process with a tunnel,
// or run a server of your own with --allow-private-webhooks to post to http://127.0.0.1.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { Rewind, WebhookVerificationFailed, verifyWebhook, type Webhook } from "@rewind.codes/sdk";

const rewind = new Rewind();
const port = Number(process.env.PORT ?? 3000);
const url = process.env.WEBHOOK_URL ?? `http://127.0.0.1:${port}/rewind-webhooks`;

// 1. Subscribe. The secret is in this answer only: a real harness stores it.
const subscription = await rewind.subscriptions.create({
  url,
  types: ["run.completed", "run.failed", "run.waiting", "quota.alert"],
  labels: { example: "webhooks" }, // only runs carrying these labels
});
const secret = subscription.secret ?? process.env.REWIND_WEBHOOK_SECRET ?? "";

let completed: () => void;
const done = new Promise<void>((resolve) => (completed = resolve));

function handle(webhook: Webhook): void {
  switch (webhook.type) {
    case "run.waiting":
      // The run waits for the outside world: a signal, an approval, a harness action.
      console.log(`waiting ${webhook.data.run.handle}, for`, webhook.data.run.waitingFor?.map((w) => w.kind));
      break;
    case "run.completed":
      // Resume the conversation that started the run: its output is here.
      console.log(`completed ${webhook.data.run.handle} for ${webhook.data.run.namespace}:`, webhook.data.run.output?.value);
      completed();
      break;
    case "run.failed":
      console.log(`failed ${webhook.data.run.handle}:`, webhook.data.run.error?.message);
      break;
    case "quota.alert":
      console.log(`${webhook.data.namespace ?? "the organization"} crossed ${webhook.data.threshold} % of ${webhook.data.quota}`);
      break;
  }
}

// 2. Receive. Verify the body exactly as received, answer fast, and deduplicate on webhook-id:
//    a retried delivery keeps it.
const seen = new Set<string>();
const server = createServer(async (req, res) => {
  if (req.method !== "POST" || req.url !== "/rewind-webhooks") return void res.writeHead(404).end();
  const chunks: Buffer[] = [];
  for await (const chunk of req) chunks.push(chunk as Buffer);

  let webhook: Webhook;
  try {
    webhook = verifyWebhook(Buffer.concat(chunks), req.headers, secret);
  } catch (error) {
    if (error instanceof WebhookVerificationFailed) return void res.writeHead(400).end();
    throw error;
  }
  res.writeHead(204).end();

  const id = String(req.headers["webhook-id"]);
  if (seen.has(id)) return;
  seen.add(id);
  handle(webhook);
});
server.listen(port);

// 3. A run that waits for a signal, then completes once it gets it.
const user = rewind.namespace("user_ada");
const run = await user.runs.start({
  code: await readFile(new URL("./workflows/greet.ts", import.meta.url), "utf8"),
  labels: { example: "webhooks" },
});
setTimeout(() => void user.runs.signal(run.id, "hello", { name: "Ada" }), 2_000);

await done;
server.close();
await rewind.subscriptions.delete(subscription.id);
