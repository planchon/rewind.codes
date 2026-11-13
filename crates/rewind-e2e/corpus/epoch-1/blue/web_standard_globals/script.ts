import { audit } from "rewind:tools";

export default async function main() {
  const url = new URL("../items?q=1#top", "https://API.example.com/v1/x");
  url.searchParams.append("tag", "a b");
  const params = new URLSearchParams("b=2&a=1&a=3");
  params.sort();
  const encoded = new TextEncoder().encode("café 👋");
  const copy = structuredClone({ when: new Date(0), nested: [1, { a: 2 }], set: new Set([1, 2]) });
  const sample = {
    href: url.href,
    origin: url.origin,
    params: params.toString(),
    all: params.getAll("a"),
    bytes: Array.from(encoded),
    decoded: new TextDecoder().decode(encoded),
    utf16: new TextDecoder("utf-16le").decode(new Uint8Array([104, 0, 105, 0])),
    base64: btoa("rewind"),
    binary: atob("cmV3aW5k"),
    clone: [copy.when.toISOString(), copy.nested, [...copy.set]],
    iterator: Iterator.from([1, 2, 3, 4]).filter((n) => n % 2 === 0).map((n) => n * 10).toArray(),
    float16: Array.from(new Float16Array([1.337, 65504])),
  };
  await audit.record(sample);
  return sample;
}
