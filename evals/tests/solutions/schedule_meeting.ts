import { office } from "rewind:tools";

type Availability = { busy: { start: string; end: string }[] };

export default async function main() {
  const people = ["ana", "ben", "chloe"];
  const calendars = await Promise.all(
    people.map(async (person) =>
      (await office.get_availability({ person, date: "2026-03-03" })).json<Availability>(),
    ),
  );
  const busy = calendars.flatMap((calendar) =>
    calendar.busy.map((slot) => [Date.parse(slot.start), Date.parse(slot.end)] as const),
  );
  const day = Date.parse("2026-03-03T00:00:00Z");
  const hour = 3_600_000;
  for (let start = day + 9 * hour; start + hour / 2 <= day + 17 * hour; start += hour / 4) {
    const end = start + hour / 2;
    if (busy.every(([from, to]) => end <= from || start >= to)) {
      const slot = { start: new Date(start).toISOString(), end: new Date(end).toISOString() };
      await office.create_event({ title: "Design review", ...slot, attendees: people });
      return slot;
    }
  }
  return null;
}
