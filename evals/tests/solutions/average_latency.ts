import { ops } from "ptc:tools";

export default async function main() {
  const services = ["api", "web", "worker"] as const;
  const metrics = await Promise.all(
    services.map((service) => ops.get_metric({ service, metric: "latency_p95" })),
  );
  return metrics.reduce((sum, metric) => sum + metric.value, 0) / metrics.length;
}
