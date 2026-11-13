import { audit } from "rewind:tools";

export default async function main() {
  audit.record({ event: "triaged" }); // no await
  return "done";
}
