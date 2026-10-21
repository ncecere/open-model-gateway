import { anthropicBaseUrl, inferenceEndpoint } from "../lib/inference-url";
import type { ModelProtocol } from "../lib/api";
import { protocolEndpoints, protocolLabel } from "../lib/model-setup";
import { Stack, useInferenceBaseUrl } from "./ui";
import { CopyField } from "./ui/copy-field/copy-field";

export { inferenceEndpoint } from "../lib/inference-url";

export function InferenceBaseField() {
  const base = useInferenceBaseUrl();
  return <Stack gap={3}>
    <CopyField label="OpenAI-compatible base URL" value={base} description="Use this base URL with your API key in an OpenAI-compatible SDK. See a model's page for its supported endpoints." />
    <CopyField label="Anthropic SDK base URL" value={anthropicBaseUrl(base)} description="The Anthropic SDK appends /v1/messages, so its base omits only the final /v1. Use your API key and a model that supports Messages." />
  </Stack>;
}

export function InferenceEndpointField({ protocol, base }: { protocol: ModelProtocol; base: string }) {
  return <>
    {protocol === "batches" && <CopyField label="Files endpoint" value={inferenceEndpoint("/v1/files", base)} description="First upload the JSONL file with purpose=batch, then create the batch using its file ID." />}
    <CopyField label="Endpoint" name={`${protocolLabel(protocol)} endpoint`} value={inferenceEndpoint(protocolEndpoints[protocol].path, base, protocol)} />
  </>;
}
