import { storage } from "ptc:tools";

export default async function main(input: { size: number }) {
  const blob = "x".repeat(input.size);
  const stored = await storage.put({ blob });
  return { length: stored.blob.length };
}
