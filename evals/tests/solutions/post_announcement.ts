import { chat } from "ptc:tools";

export default async function main() {
  await chat.post_message({ channel: "#ops", text: "Deploy freeze starts Friday 18:00 UTC" });
  return "posted";
}
