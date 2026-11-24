import { sleep, CancelledError } from "rewind";

export default async function main() {
  try {
    await sleep("30d");
    return "slept";
  } catch (error) {
    if (error instanceof CancelledError) return `cleaned up after: ${error.message}`;
    throw error;
  }
}
