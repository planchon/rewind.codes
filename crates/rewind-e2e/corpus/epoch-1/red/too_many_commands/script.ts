import { audit } from "rewind:actions";

export default async function main() {
  await Promise.all(Array.from({ length: 50 }, (_, i) => audit.record({ i })));
}
