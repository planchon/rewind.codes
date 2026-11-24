import { tracker } from "rewind:actions";

export default async function main() {
  return await tracker.create_issue({
    repo: "acme/web",
    title: "Login button misaligned on Safari",
    labels: ["bug"],
  });
}
