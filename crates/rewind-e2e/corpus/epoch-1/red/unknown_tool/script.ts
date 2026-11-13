import { jira } from "rewind:tools";

export default async function main() {
  return await jira.search({ query: "is:open" });
}
