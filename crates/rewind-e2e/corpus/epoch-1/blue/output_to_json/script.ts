import { audit } from "rewind:tools";

export default async function main() {
  return {
    toJSON() {
      audit.record({ serializing: true }); // runs when the output is serialized
      return "out";
    },
  };
}
