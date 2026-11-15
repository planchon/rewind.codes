// End users: one namespace each, tokens for the agents acting for them, and quotas.
//
//   pnpm end-users
import { Rewind, isProblem } from "@rewind.codes/sdk";

// The harness's backend holds the API key: it reaches every namespace.
const rewind = new Rewind();
const toolset = await rewind.toolsets.create({ http: { allow: ["api.github.com"] } });
const script = `export default async function main(input: { n: number }) {
  return { squares: Array.from({ length: input.n }, (_, i) => i * i) };
}`;

// 1. Limits: defaults for every end user, and exceptions for one (a paying user, a blocked one).
await rewind.namespaces.defaults.put({ defaults: { activeRuns: 5, runsPerMonth: 1_000, maxTtl: "7d" } });
await rewind.namespaces.put("user_chen", { exceptions: { activeRuns: 20, runsPerMonth: 10_000 } });
await rewind.namespaces.put("user_erin", { exceptions: { activeRuns: 0 } });

// 2. The backend names the end user of each request. Bob's run lives in Bob's namespace.
const bobs = await rewind.namespace("user_bob").runs.start({ toolset: toolset.id, script, input: { n: 3 }, wait: "10s" });

// 3. The agent acting for Ada gets a token of her namespace, minted by the backend.
const { token, id: tokenId } = await rewind.namespaces.tokens.create("user_ada", {
  ttl: "1h",
  labels: { conversation: "c-7" }, // the runs it starts carry them, and it reaches only those
});

// In the agent, possibly another process: everything it does, and every script its model
// writes, stays in Ada's namespace.
const asAda = new Rewind({ url: rewind.url, token });
const adas = await asAda.runs.start({ toolset: toolset.id, script, input: { n: 4 }, wait: "10s" });
console.log(`Ada's run ${adas.id} in ${adas.namespace}:`, adas.output?.value);

try {
  await asAda.runs.get(bobs.id);
} catch (error) {
  if (isProblem(error, "not-found")) console.log("Bob's run does not exist for Ada's token (404)");
  else throw error;
}

// 4. A blocked end user starts nothing, with an answer the agent can show.
try {
  await rewind.namespace("user_erin").runs.start({ toolset: toolset.id, script, input: { n: 1 } });
} catch (error) {
  if (isProblem(error, "quota-exceeded")) console.log("Erin is blocked:", error.quota);
  else throw error;
}

// 5. What each end user uses this month, and who is near a limit.
const ada = await rewind.namespaces.get("user_ada");
console.log("Ada's limits:", ada.namespace.limits, "use:", ada.namespace.usage, "tokens:", ada.tokens);
const near = await rewind.namespaces.list({ above: 0.8, order: "runs" });
console.log(`${near.namespaces.length} end users at 80 % of a limit or more`);

// 6. Revoking: one token, or every token of an end user.
await rewind.namespaces.tokens.revoke(tokenId);
await rewind.namespaces.tokens.revokeAll("user_ada");
