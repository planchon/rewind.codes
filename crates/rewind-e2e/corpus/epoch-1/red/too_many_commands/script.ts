import { audit } from "rewind:tools";

export default async function main() {
  await Promise.all(Array.from({ length: 50 }, (_, i) => audit.record({ i })));
}
