import { http, signalUrl } from "rewind:actions";

export default async function main() {
  const hook = await signalUrl({ name: "done" });
  const response = await http({ url: "https://api.example.com/" });
  return [hook, response];
}
