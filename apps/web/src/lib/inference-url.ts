/** Public presentation only; never used as a fetch target. */
export function anthropicBaseUrl(base: string) {
  return base.replace(/\/v1$/, "");
}

export function inferenceEndpoint(text: string, base: string, protocol?: string) {
  const endpointBase = protocol === "realtime" ? base.replace(/^https:/, "wss:").replace(/^http:/, "ws:") : base;
  return text.replace(/\/v1(?=\/)/g, () => endpointBase);
}
