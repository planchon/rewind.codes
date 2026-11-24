import { jira } from "rewind:actions";

export default async function main() {
  return await jira.search({ query: "is:open" });
}
