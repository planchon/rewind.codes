import { audit } from "ptc:tools";

export default async function main() {
  audit.record({ event: "triaged" }); // no await
  return "done";
}
