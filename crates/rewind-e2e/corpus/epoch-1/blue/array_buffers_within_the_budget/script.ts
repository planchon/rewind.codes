import { sleep } from "rewind";

// Keeps buffers of `size` bytes until the instance's budget refuses one.
function hoard(size: number): number {
  const kept: Uint8Array[] = [];
  try {
    for (;;) kept.push(new Uint8Array(size));
  } catch (error) {
    if (!(error instanceof RangeError)) throw error;
    return kept.length;
  }
}

export default async function main() {
  const before = hoard(16 << 20);
  await sleep("1h");
  const growable = new ArrayBuffer(4, { maxByteLength: 1 << 20 });
  growable.resize(1024);
  const moved = growable.transfer();
  const copy = structuredClone(moved);
  copy.resize(2048);
  let tooLarge = "";
  try {
    new ArrayBuffer(0, { maxByteLength: 2 ** 36 });
  } catch (error) {
    tooLarge = String(error);
  }
  return {
    before,
    after: hoard(16 << 20),
    moved: [moved.byteLength, moved.maxByteLength, growable.detached],
    copy: [copy.byteLength, copy.resizable],
    tooLarge,
  };
}
