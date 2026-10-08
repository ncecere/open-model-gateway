/*
 * Amazon Bedrock connection form parts: region choice, AWS identity mode and the
 * optional VPC endpoint. Mirrors the server's checks (providers/bedrock/auth.rs) for
 * early feedback only; the server validates again and enforces its allowlists
 * (GATEWAY_AWS_PROFILE_ALLOWLIST, GATEWAY_BEDROCK_ENDPOINT_ALLOWLIST). References
 * (profile name, role ARN, external ID) are write-only: reads return the mode alone.
 */
import type { Field, Option, Values } from "./forms";

export type AwsAuthMode = "default" | "profile" | "role";

/** Common Bedrock Runtime regions. Any other valid region code can be entered. */
export const bedrockRegions: Option[] = [
  ["us-east-1", "US East (N. Virginia)"], ["us-east-2", "US East (Ohio)"], ["us-west-2", "US West (Oregon)"],
  ["ca-central-1", "Canada (Central)"], ["sa-east-1", "South America (São Paulo)"],
  ["eu-central-1", "Europe (Frankfurt)"], ["eu-west-1", "Europe (Ireland)"], ["eu-west-2", "Europe (London)"], ["eu-west-3", "Europe (Paris)"], ["eu-north-1", "Europe (Stockholm)"],
  ["ap-northeast-1", "Asia Pacific (Tokyo)"], ["ap-northeast-2", "Asia Pacific (Seoul)"], ["ap-south-1", "Asia Pacific (Mumbai)"], ["ap-southeast-1", "Asia Pacific (Singapore)"], ["ap-southeast-2", "Asia Pacific (Sydney)"],
  ["us-gov-west-1", "AWS GovCloud (US-West)"],
].map(([value, name]) => ({ value, label: `${name} · ${value}` }));
const OTHER = "other";

export const awsAuthOptions: { value: AwsAuthMode; label: string }[] = [
  { value: "default", label: "Server AWS identity (default credential chain)" },
  { value: "profile", label: "Named AWS profile" },
  { value: "role", label: "Assume IAM role" },
];
const authHelp: Record<AwsAuthMode, string> = {
  default: "Task role, instance profile or other default-chain credentials on the server.",
  profile: "A profile in the server's AWS config. It must be on the server's profile allowlist.",
  role: "The server's identity calls STS AssumeRole for this role.",
};

const chars = (value: string, extra: string) => [...value].every(c => /[A-Za-z0-9]/.test(c) || extra.includes(c));
export const validRegion = (v: string) => v.length <= 32 && /^[a-z]{2}(-[a-z]+)+-\d{1,2}$/.test(v);
export const validProfileName = (v: string) => v.length >= 1 && v.length <= 64 && !v.startsWith("-") && chars(v, "_.+-");
export function validRoleArn(v: string) {
  const m = /^arn:aws(-[a-z]+)*:iam::\d{12}:role\/(.+)$/.exec(v);
  if (!m || v.length > 2048) return false;
  const segments = m[2].split("/"), name = segments[segments.length - 1];
  return name.length >= 1 && name.length <= 64 && segments.every(s => s.length > 0 && chars(s, "_+=,.@-")) && m[2].length - name.length <= 511;
}
export const validExternalId = (v: string) => v.length >= 2 && v.length <= 1224 && chars(v, "_+=,.@:/-");
export const validSessionName = (v: string) => v.length >= 2 && v.length <= 64 && chars(v, "_+=,.@-");
/** An exact HTTPS origin: no credentials, path, query or fragment. */
export function endpointError(v: string) {
  try {
    const u = new URL(v);
    if (u.protocol !== "https:" || u.username || u.password || u.search || u.hash || u.pathname !== "/" || v.endsWith("/") || !/^[A-Za-z0-9.-]+$/.test(u.hostname)) return "Use an HTTPS origin only, such as https://vpce-….amazonaws.com.";
  } catch { return "Enter an HTTPS URL."; }
}

/** Bedrock `modelId`: a model ID, an inference profile ID (us.…, global.…) or a Bedrock ARN in the connection's region. */
export function bedrockUpstreamError(model: string, region: string | null | undefined) {
  const message = "Use a model ID, inference profile ID (us.…, eu.…, global.…) or a Bedrock ARN in this connection's region.";
  if (!model.startsWith("arn:")) return model.length <= 256 && /^[A-Za-z0-9][A-Za-z0-9._:-]*$/.test(model) ? undefined : message;
  const parts = model.split(":"), [, partition, service, arnRegion, account] = parts, resource = parts.slice(5).join(":");
  const slash = resource.indexOf("/"), kind = resource.slice(0, slash), id = resource.slice(slash + 1);
  const kinds = ["foundation-model", "inference-profile", "application-inference-profile", "provisioned-model", "custom-model-deployment", "imported-model", "prompt"];
  const ok = parts.length >= 6 && /^aws(-[a-z]+)*$/.test(partition) && service === "bedrock" && arnRegion === region && slash > 0 && kinds.includes(kind)
    && (/^\d{12}$/.test(account) || kind === "foundation-model" && account === "") && id.length >= 1 && id.length <= 256 && /^[A-Za-z0-9][A-Za-z0-9._:/-]*$/.test(id);
  return ok ? undefined : message;
}

/**
 * Identity and endpoint fields. `when` scopes them (the Add connection form shows them
 * for Bedrock only). Stored references are never returned, so editing restates them.
 */
export function awsAccessFields(when: (v: Values) => boolean = () => true, current?: { aws_auth?: AwsAuthMode | null; endpoint?: string | null }): Field[] {
  const mode = (v: Values) => when(v) && v.aws_auth;
  return [
    { name: "aws_auth", label: "AWS identity", type: "select", required: true, value: current?.aws_auth ?? "default", options: awsAuthOptions, visibleWhen: when, helpFor: v => authHelp[v.aws_auth as AwsAuthMode] },
    { name: "aws_profile", label: "Profile name", required: true, maxLength: 64, placeholder: "bedrock-prod", visibleWhen: v => mode(v) === "profile", validate: v => validProfileName(v) ? undefined : "Use letters, digits, dot, underscore, plus or hyphen." },
    { name: "aws_role_arn", label: "Role ARN", required: true, maxLength: 2048, placeholder: "arn:aws:iam::123456789012:role/bedrock-invoke", visibleWhen: v => mode(v) === "role", validate: v => validRoleArn(v) ? undefined : "Enter an IAM role ARN." },
    { name: "aws_external_id", label: "External ID", maxLength: 1224, visibleWhen: v => mode(v) === "role", help: "Only if the role's trust policy requires one.", validate: v => validExternalId(v) ? undefined : "Use 2–1224 letters, digits or + = , . @ : / - _." },
    { name: "aws_session_name", label: "Session name", maxLength: 64, placeholder: "open-model-gateway", visibleWhen: v => mode(v) === "role", validate: v => validSessionName(v) ? undefined : "Use 2–64 letters, digits or + = , . @ - _." },
    { name: "aws_endpoint", label: "VPC endpoint", value: current?.endpoint ?? "", maxLength: 2048, placeholder: "https://vpce-….bedrock-runtime.<region>.vpce.amazonaws.com", visibleWhen: when, help: "Leave empty for the regional endpoint. Must be on the server's endpoint allowlist.", validate: endpointError },
  ];
}
/** The region select plus an "Other" code. */
export function regionFields(when: (v: Values) => boolean, current?: string | null): Field[] {
  const listed = !current || bedrockRegions.some(r => r.value === current);
  return [
    { name: "region", label: "AWS region", type: "select", required: true, value: listed ? current ?? "us-east-1" : OTHER, options: [...bedrockRegions, { value: OTHER, label: "Other region…" }], visibleWhen: when },
    { name: "region_other", label: "Region code", required: true, maxLength: 32, value: listed ? "" : current ?? "", placeholder: "eu-south-2", visibleWhen: v => when(v) && v.region === OTHER, validate: v => validRegion(v) ? undefined : "Enter an AWS region code such as eu-south-2." },
  ];
}
export const regionValue = (v: Values) => v.region === OTHER ? v.region_other : v.region;

/** API fields for the identity and endpoint: `credential_ref` plus write-only role options. */
export function awsAccessBody(v: Values) {
  const mode = v.aws_auth as AwsAuthMode, role = mode === "role";
  return {
    credential_ref: mode === "profile" ? `aws:profile:${v.aws_profile}` : role ? `aws:role:${v.aws_role_arn}` : "aws:default",
    aws_external_id: role && v.aws_external_id ? v.aws_external_id : null,
    aws_session_name: role && v.aws_session_name ? v.aws_session_name : null,
    endpoint: v.aws_endpoint || null,
  };
}
export const awsAuthLabel = (mode?: AwsAuthMode | null) => mode === "profile" ? "Named AWS profile · hidden" : mode === "role" ? "Assumed IAM role · hidden" : mode === "default" ? "Server AWS identity" : "AWS identity";
