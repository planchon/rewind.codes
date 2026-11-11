export default async function main() {
  const ticks: number[] = [];
  await new Promise<void>((resolve) => {
    const id = setInterval(() => {
      ticks.push(performance.now());
      if (ticks.length === 3) {
        clearInterval(id);
        resolve();
      }
    }, 1000);
  });
  return ticks;
}
