export default async function main() {
  const response = await fetch("https://api.example.com/v1/items");
  return response.status;
}
