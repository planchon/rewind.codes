import { tracker } from "ptc:tools";

export default async function main() {
  return await tracker.create_issue({
    repo: "acme/web",
    title: "Login button misaligned on Safari",
    labels: ["bug"],
  });
}
