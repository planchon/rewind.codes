export default async function main() {
  await new Promise((resolve) => setTimeout(resolve, 60_000));
  return "woke up";
}
