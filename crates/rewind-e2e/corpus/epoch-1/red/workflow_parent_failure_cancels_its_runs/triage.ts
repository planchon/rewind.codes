import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const approval = await signal<{ close: boolean }>("approval");
  return { closed: approval.close ? input.repo.length : 0 };
}
