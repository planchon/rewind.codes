/** What `run_script` returns of a run's output at most: 256 KiB, the bound of the API's inline values. */
export const OUTPUT_LIMIT = 256 * 1024;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** The size of a text in UTF-8, in bytes. */
export function utf8Length(text: string): number {
  return encoder.encode(text).length;
}

/** A window of a text, in UTF-8 bytes, cut on character boundaries. */
export interface Slice {
  text: string;
  /** The byte offset of the window's start. */
  start: number;
  /** The byte offset just past the window's end: where the next window starts. */
  end: number;
  /** The size of the whole text, in bytes. */
  total: number;
}

/** At most `limit` bytes of `text` from the byte offset `start`, without cutting a character. */
export function sliceUtf8(text: string, start: number, limit: number): Slice {
  const bytes = encoder.encode(text);
  const total = bytes.length;
  let from = Math.min(Math.max(0, Math.floor(start)), total);
  while (from < total && isContinuation(bytes[from])) {
    from += 1;
  }
  let to = Math.min(total, from + Math.max(0, Math.floor(limit)));
  while (to < total && to > from && isContinuation(bytes[to])) {
    to -= 1;
  }
  return { text: decoder.decode(bytes.subarray(from, to)), start: from, end: to, total };
}

/**
 * A text cut to at most `limit` bytes by leaving out its middle: a quarter of the room for its
 * start, the rest for its end, where the most recent lines of a trace are.
 */
export function truncateMiddle(text: string, limit: number): string {
  const total = utf8Length(text);
  if (total <= limit) {
    return text;
  }
  const marker = (left: number) => `\n[… ${left} bytes left out …]\n`;
  const room = Math.max(0, limit - utf8Length(marker(total)));
  const head = sliceUtf8(text, 0, Math.floor(room / 4));
  const tail = sliceUtf8(text, total - (room - (head.end - head.start)), room);
  return `${head.text}${marker(tail.start - head.end)}${tail.text}`;
}

/** A text cut to at most `limit` bytes, saying how much was left out. */
export function truncateEnd(text: string, limit: number): string {
  const slice = sliceUtf8(text, 0, limit);
  return slice.end === slice.total ? text : `${slice.text}[… ${slice.total - slice.end} more bytes]`;
}

function isContinuation(byte: number | undefined): boolean {
  return byte !== undefined && (byte & 0xc0) === 0x80;
}
