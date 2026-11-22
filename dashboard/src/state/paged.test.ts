// What a list shows of its answers (archi/05, "Nothing moves on its own", D7): at its top an
// answer applies at once; scrolled, the rows keep their place with their new values and the
// answer waits behind "N new · Show"; "Load more" adds the new tail.
import { describe, expect, it } from "vitest";
import { type Shown, showRows } from "./paged";

interface Row {
  id: string;
  cpu: number;
}

const list = {};
const keyOf = (row: Row) => row.id;
const how = { keyOf, stale: false, atTop: false, loadingMore: false, shownAnswer: null };
const none: Shown<Row> = { resource: null, answer: null, keys: [], rows: [], pending: null };

function first(answer: Row[]): Shown<Row> {
  return showRows(none, list, answer, how);
}

describe("a list's rows, as shown", () => {
  const a = { id: "a", cpu: 3 };
  const b = { id: "b", cpu: 2 };
  const shown = first([a, b]);

  it("shows the first answer as it is", () => {
    expect(shown.rows).toEqual([a, b]);
    expect(shown.pending).toBeNull();
  });

  it("returns what it showed for the same answer", () => {
    expect(showRows(shown, list, shown.answer, how)).toBe(shown);
  });

  it("keeps the rows' place while scrolled, each with its new values, and counts the new ones", () => {
    const answer = [
      { id: "c", cpu: 9 },
      { id: "b", cpu: 5 },
      { id: "a", cpu: 4 },
    ];
    const held = showRows(shown, list, answer, how);
    expect(held.rows.map((r) => [r.id, r.cpu])).toEqual([
      ["a", 4],
      ["b", 5],
    ]);
    expect(held.pending).toEqual({ added: 1 });
  });

  it("keeps a dropped row with its last values until the answer shows", () => {
    const held = showRows(shown, list, [{ id: "b", cpu: 7 }], how);
    expect(held.rows.map((r) => [r.id, r.cpu])).toEqual([
      ["a", 3],
      ["b", 7],
    ]);
    expect(held.pending).toEqual({ added: 0 });
  });

  it("applies an answer at once at the top, or once the operator asks for it", () => {
    const answer = [b, a];
    expect(showRows(shown, list, answer, { ...how, atTop: true }).rows).toEqual([b, a]);
    expect(showRows(shown, list, answer, { ...how, shownAnswer: answer }).rows).toEqual([b, a]);
  });

  it('adds the new tail after "Load more", without moving the rows shown', () => {
    const c = { id: "c", cpu: 1 };
    const answer = [b, a, c];
    const more = showRows(shown, list, answer, { ...how, loadingMore: true });
    expect(more.rows.map((r) => r.id)).toEqual(["a", "b", "c"]);
  });

  it("shows another list's rows as they are", () => {
    const other = {};
    expect(showRows(shown, other, [b, a], how).rows).toEqual([b, a]);
  });
});
