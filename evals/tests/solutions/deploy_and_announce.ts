import { chat, ops } from "rewind:actions";
import { sleep } from "rewind";

type Deployment = { deployment_id: string; status: string };

export default async function main() {
  const deployment = (await ops.start_deployment({ service: "api", version: "2.4.1" })).json<Deployment>();
  for (;;) {
    await sleep("10m");
    const state = (await ops.get_deployment({ deployment_id: deployment.deployment_id })).json<Deployment>();
    if (state.status === "succeeded") break;
  }
  await chat.post_message({ channel: "#deploys", text: "api 2.4.1 is live" });
  return "live";
}
