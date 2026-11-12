import { reports } from "ptc:tools";

export default async function main() {
  const report = await reports.build({ month: "2026-10" });
  return { pages: report.pages };
}
