export default async function main() {
  const response = await fetch("https://metadata.internal/latest");
  return response.status;
}
