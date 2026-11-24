export default async function main() {
  const tools = await import("rewind:actions");
  return Object.keys(tools);
}
