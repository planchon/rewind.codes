export default async function main() {
  const tools = await import("ptc:tools");
  return Object.keys(tools);
}
