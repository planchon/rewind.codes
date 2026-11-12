export default async function main() {
  const filled = new Array(2 ** 27).fill(1.5);
  return filled.length;
}
