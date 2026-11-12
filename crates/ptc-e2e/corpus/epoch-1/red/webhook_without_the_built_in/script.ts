import { webhook } from "ptc";

export default async function main() {
  const hook = await webhook("deploy_done");
  return hook.url;
}
