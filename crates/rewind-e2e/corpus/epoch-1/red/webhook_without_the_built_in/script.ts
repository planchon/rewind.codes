import { webhook } from "rewind";

export default async function main() {
  const hook = await webhook("deploy_done");
  return hook.url;
}
