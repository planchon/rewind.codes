export default async function main() {
  const tools = await import("rewind:tools");
  return Object.keys(tools);
}
