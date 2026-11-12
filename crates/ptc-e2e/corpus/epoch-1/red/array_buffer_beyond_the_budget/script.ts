export default async function main() {
  const buffer = new ArrayBuffer(2 ** 36);
  return buffer.byteLength;
}
