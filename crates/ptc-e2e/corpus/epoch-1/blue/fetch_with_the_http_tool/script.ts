export default async function main() {
  const created = await fetch("https://api.example.com/v1/items", {
    method: "post",
    headers: { "Content-Type": "application/json", "X-Request-Id": "r-1" },
    body: JSON.stringify({ name: "widget" }),
  });
  const item = await created.json();
  const missing = await fetch(new URL("https://api.example.com/v1/items/404"));
  const logo = await fetch("https://api.example.com/logo.png");
  const bytes = await logo.bytes();
  return {
    created: [created.status, created.ok, created.headers.get("location"), item.id],
    missing: [missing.status, missing.ok, missing.statusText, await missing.text()],
    logo: [logo.headers.get("content-type"), Array.from(bytes)],
  };
}
