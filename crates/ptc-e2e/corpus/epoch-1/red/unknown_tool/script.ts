import { jira } from "ptc:tools";

export default async function main() {
  return await jira.search({ query: "is:open" });
}
