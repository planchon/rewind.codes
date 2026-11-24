import { audit } from "rewind:actions";

export default async function main() {
  const all = await audit.list();
  const again = await audit.list(undefined);
  let refused = "";
  try {
    await audit.get();
  } catch (error) {
    refused = `${(error as Error).name}: ${(error as Error).message}`;
  }
  return { all, again, refused };
}
