import { payments } from "ptc:tools";
import { ToolError } from "ptc";

export default async function main() {
  try {
    await payments.charge({ amount: 100 });
    return { charged: true, checked: false };
  } catch (error) {
    if (error instanceof ToolError && error.outcomeUnknown) {
      const status = await payments.status({ amount: 100 });
      return { charged: status.charged, checked: true };
    }
    throw error;
  }
}
