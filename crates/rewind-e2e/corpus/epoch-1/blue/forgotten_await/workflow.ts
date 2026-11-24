import { audit } from "rewind:actions";

export default async function main() {
  audit.record({ event: "triaged" }); // no await
  return "done";
}
