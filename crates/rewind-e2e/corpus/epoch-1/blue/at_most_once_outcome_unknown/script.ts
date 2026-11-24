import { payments } from "rewind:actions";
import { ActionError } from "rewind";

export default async function main(input: { amount: number }) {
  try {
    await payments.charge({ amount: input.amount });
    return { charged: true };
  } catch (error) {
    if (error instanceof ActionError && error.outcomeUnknown) {
      const status = await payments.status({ amount: input.amount });
      return { charged: status.charged, outcomeUnknown: true, message: error.message };
    }
    throw error;
  }
}
