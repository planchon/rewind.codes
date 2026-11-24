import { http, webhook } from "rewind:actions";

export default async function main() {
  const hook = await webhook({ name: "done" });
  const response = await http({ url: "https://api.example.com/" });
  return [hook, response];
}
