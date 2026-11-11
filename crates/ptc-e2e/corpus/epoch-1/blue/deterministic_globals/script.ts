import { audit } from "ptc:tools";
import { run } from "ptc";

export default async function main() {
  const sample = {
    now: Date.now(),
    date: new Date().toISOString(),
    called: Date(),
    random: Math.random(),
    uuid: crypto.randomUUID(),
    bytes: Array.from(crypto.getRandomValues(new Uint8Array(8))),
    perf: performance.now(),
    formatted: new Intl.DateTimeFormat("en-US", { dateStyle: "long" }).format(),
    locale: (1234567.891).toLocaleString(),
    sorted: ["é", "e", "z", "a"].sort((a, b) => a.localeCompare(b)),
    runId: run.id,
    startedAt: run.startedAt.toISOString(),
  };
  await audit.record(sample);
  return sample;
}
