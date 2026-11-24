import { audit } from "rewind:actions";

export default async function main() {
  return {
    toJSON() {
      audit.record({ serializing: true }); // runs when the output is serialized
      return "out";
    },
  };
}
