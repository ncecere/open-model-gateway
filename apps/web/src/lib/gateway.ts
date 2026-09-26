export type GatewayStatus = "ready" | "not ready";

export async function getGatewayStatus(signal: AbortSignal): Promise<GatewayStatus> {
  const response = await fetch("/health/ready", {
    cache: "no-store",
    signal: AbortSignal.any([signal, AbortSignal.timeout(3000)]),
  });
  const body: unknown = await response.json();
  const status = typeof body === "object" && body !== null && "status" in body ? body.status : undefined;

  if (response.status === 200 && status === "ready") return "ready";
  if (response.status === 503 && status === "not_ready") return "not ready";
  throw new Error("Unexpected gateway readiness response");
}
