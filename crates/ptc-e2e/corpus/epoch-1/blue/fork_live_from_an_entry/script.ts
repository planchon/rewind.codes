import { weather } from "ptc:tools";

export default async function main() {
  const paris = await weather.forecast({ city: "Paris" });
  const oslo = await weather.forecast({ city: "Oslo" });
  return { paris: paris.sky, oslo: oslo.sky };
}
