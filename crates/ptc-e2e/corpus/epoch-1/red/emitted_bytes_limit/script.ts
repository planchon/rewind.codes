export default async function main() {
  const line = "x".repeat(1024 * 1024);
  for (let i = 0; i < 100; i++) console.log(line);
}
