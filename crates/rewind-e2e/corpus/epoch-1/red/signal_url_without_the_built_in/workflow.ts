import { signalUrl } from "rewind";

export default async function main() {
  const hook = await signalUrl("deploy_done");
  return hook.url;
}
