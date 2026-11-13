import { weather } from "rewind:tools";

export default async function main(input: { city: string }) {
  const asked = Date.now();
  const forecast = await weather.forecast({ city: input.city });
  return { forecast, waited: Date.now() - asked > 0 };
}
