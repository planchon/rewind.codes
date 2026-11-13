import { slack } from "rewind:tools";

export default async function main() {
  await slack.post_message({ channel: "#triage", text: 42 } as unknown as { channel: string; text: string });
}
