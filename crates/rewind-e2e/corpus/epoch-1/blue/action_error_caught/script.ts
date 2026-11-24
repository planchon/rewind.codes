import { payments } from "rewind:actions";
import { ActionError } from "rewind";

export default async function main() {
  try {
    await payments.charge({ amount: 100 });
    return { charged: true, checked: false };
  } catch (error) {
    if (error instanceof ActionError && error.outcomeUnknown) {
      const status = await payments.status({ amount: 100 });
      return { charged: status.charged, checked: true };
    }
    throw error;
  }
}
