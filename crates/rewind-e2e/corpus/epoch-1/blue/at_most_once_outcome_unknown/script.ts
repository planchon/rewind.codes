import { payments } from "rewind:tools";
import { ToolError } from "rewind";

export default async function main(input: { amount: number }) {
  try {
    await payments.charge({ amount: input.amount });
    return { charged: true };
  } catch (error) {
    if (error instanceof ToolError && error.outcomeUnknown) {
      const status = await payments.status({ amount: input.amount });
      return { charged: status.charged, outcomeUnknown: true, message: error.message };
    }
    throw error;
  }
}
