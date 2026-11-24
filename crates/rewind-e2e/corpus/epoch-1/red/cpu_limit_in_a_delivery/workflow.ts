import { github } from "rewind:actions";

export default async function main() {
  const issue = await github.get_issue({ number: 4 });
  while (issue.title !== "") {
    issue.title += ".";
    issue.title = issue.title.slice(0, 10);
  }
}
