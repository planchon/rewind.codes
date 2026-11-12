import { ops } from "ptc:tools";
import { signal, webhook } from "ptc";

export default async function main() {
  const hook = await webhook("build_done");
  await ops.trigger_build({ repo: "acme/api", commit: "3f2a9c1", callback_url: hook.url });
  const result = await signal<{ status: string }>("build_done");
  if (result.status !== "passed") return `the build ${result.status}`;
  await ops.start_deployment({ service: "api", version: "2.5.0" });
  return "deployed";
}
