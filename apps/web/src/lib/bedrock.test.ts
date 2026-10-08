import { describe, expect, it } from "vitest";
import { validateFields } from "./forms";
import { awsAccessBody, awsAccessFields, bedrockRegions, bedrockUpstreamError, endpointError, regionValue, validExternalId, validProfileName, validRegion, validRoleArn, validSessionName } from "./bedrock";
import { deploymentCreateAction, providerBody, providerFields } from "../pages/catalog";
import { model, provider } from "./test-fixtures";

const ROLE = "arn:aws:iam::123456789012:role/gateway/bedrock-invoke";
const VPCE = "https://vpce-0abc.bedrock-runtime.us-east-1.vpce.amazonaws.com";
const base = { name: "Bedrock", provider: "bedrock", auth_mode: "environment", credential_variable: "", endpoint: "", region: "us-east-1", region_other: "", aws_auth: "default", aws_profile: "", aws_role_arn: "", aws_external_id: "", aws_session_name: "", aws_endpoint: "", enabled: "false" };

describe("Bedrock connection form", () => {
  it("offers the common Bedrock regions plus a validated other code", () => {
    for (const region of ["us-east-1", "us-east-2", "us-west-2", "eu-central-1", "eu-west-1", "eu-west-3", "ap-northeast-1", "ap-southeast-1", "ap-southeast-2", "ap-south-1", "ca-central-1", "sa-east-1"]) expect(bedrockRegions.some(r => r.value === region)).toBe(true);
    expect(validateFields(providerFields(), base)).toEqual({});
    expect(validateFields(providerFields(), { ...base, region: "other", region_other: "eu-south-2" })).toEqual({});
    expect(regionValue({ ...base, region: "other", region_other: "eu-south-2" })).toBe("eu-south-2");
    for (const bad of ["", "eu-south", "EU-SOUTH-2", "eu-south-2/x", "e-south-2"]) expect(validateFields(providerFields(), { ...base, region: "other", region_other: bad })).toHaveProperty("region_other");
    expect(validRegion("us-gov-west-1")).toBe(true);
    // An unlisted stored region opens as "Other" with its code.
    const fields = providerFields({ ...provider, provider: "bedrock", region: "il-central-1" });
    expect(fields.find(f => f.name === "region")!.value).toBe("other");
    expect(fields.find(f => f.name === "region_other")!.value).toBe("il-central-1");
  });

  it("builds each identity reference and sends role options only with a role", () => {
    expect(providerBody(base)).toEqual({ name: "Bedrock", provider: "bedrock", credential_ref: "aws:default", aws_external_id: null, aws_session_name: null, endpoint: null, region: "us-east-1", enabled: false });
    expect(providerBody({ ...base, aws_auth: "profile", aws_profile: "bedrock-prod", aws_external_id: "stale" })).toMatchObject({ credential_ref: "aws:profile:bedrock-prod", aws_external_id: null });
    expect(providerBody({ ...base, aws_auth: "role", aws_role_arn: ROLE, aws_external_id: "tenant-42", aws_session_name: "gw", aws_endpoint: VPCE })).toMatchObject({ credential_ref: `aws:role:${ROLE}`, aws_external_id: "tenant-42", aws_session_name: "gw", endpoint: VPCE });
    expect(awsAccessBody({ aws_auth: "role", aws_role_arn: ROLE, aws_external_id: "", aws_session_name: "", aws_endpoint: "" })).toEqual({ credential_ref: `aws:role:${ROLE}`, aws_external_id: null, aws_session_name: null, endpoint: null });
    // Non-Bedrock bodies never carry AWS fields.
    expect(providerBody({ ...base, provider: "openai", credential_variable: "OPENAI_KEY" })).not.toHaveProperty("aws_external_id");
  });

  it("validates identities and the VPC endpoint before submission", () => {
    expect(validateFields(providerFields(), { ...base, aws_auth: "role" })).toHaveProperty("aws_role_arn");
    expect(validateFields(providerFields(), { ...base, aws_auth: "role", aws_role_arn: "arn:aws:iam::123:user/x" })).toHaveProperty("aws_role_arn");
    expect(validateFields(providerFields(), { ...base, aws_auth: "role", aws_role_arn: ROLE, aws_external_id: "x" })).toHaveProperty("aws_external_id");
    expect(validateFields(providerFields(), { ...base, aws_auth: "profile" })).toHaveProperty("aws_profile");
    expect(validateFields(providerFields(), { ...base, aws_endpoint: "http://bedrock.internal" })).toHaveProperty("aws_endpoint");
    expect(validateFields(providerFields(), { ...base, aws_endpoint: VPCE })).toEqual({});
    for (const bad of [`${VPCE}/`, `${VPCE}/path`, `${VPCE}?q=1`, "https://user:pw@host.example", "vpce.example"]) expect(endpointError(bad)).toBeDefined();
    expect(validRoleArn("arn:aws-us-gov:iam::123456789012:role/r")).toBe(true);
    expect(validRoleArn("arn:aws:iam::123456789012:role/a//b")).toBe(false);
    expect(validProfileName("-p")).toBe(false); expect(validProfileName("prod.bedrock+1")).toBe(true);
    expect(validExternalId("a:b/c=d")).toBe(true); expect(validExternalId("has space")).toBe(false);
    expect(validSessionName("team-a.gw")).toBe(true); expect(validSessionName("a:b")).toBe(false);
    // The settings dialog (no profile field) shows the identity fields unconditionally.
    expect(validateFields(awsAccessFields(), { aws_auth: "role", aws_role_arn: ROLE, aws_external_id: "", aws_session_name: "", aws_endpoint: "" })).toEqual({});
    // Other profiles never validate Bedrock fields.
    expect(validateFields(providerFields(), { ...base, provider: "openai", credential_variable: "OPENAI_KEY", aws_auth: "role", aws_endpoint: "http://x" })).toEqual({});
  });

  it("checks Bedrock route model IDs, inference profiles and same-region ARNs", () => {
    for (const ok of ["anthropic.claude-3-5-sonnet-20240620-v1:0", "us.anthropic.claude-3-7-sonnet-20250219-v1:0", "global.anthropic.claude-sonnet-4-20250514-v1:0", "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-3-7-sonnet-20250219-v1:0", "arn:aws:bedrock:us-east-1::foundation-model/amazon.nova-pro-v1:0"]) expect(bedrockUpstreamError(ok, "us-east-1")).toBeUndefined();
    for (const bad of ["model with space", "arn:aws:bedrock:eu-west-1:123456789012:inference-profile/eu.x", "arn:aws:bedrock:us-east-1::inference-profile/us.x", "arn:aws:bedrock:us-east-1:123456789012:knowledge-base/kb"]) expect(bedrockUpstreamError(bad, "us-east-1")).toBeDefined();
    const bedrock = { ...provider, id: "bedrock-connection", provider: "bedrock", region: "us-east-1" };
    const fields = deploymentCreateAction({ models: [model], providers: [provider, bedrock] }).fields!, upstream = fields.find(f => f.name === "upstream_model")!;
    const values = { model_id: model.id, provider_connection_id: bedrock.id, upstream_model: "arn:aws:bedrock:eu-west-1:123456789012:inference-profile/eu.x", enabled: "false" };
    expect(validateFields(fields, values)).toHaveProperty("upstream_model");
    expect(upstream.helpFor!(values)).toContain("inference profile");
    expect(validateFields(fields, { ...values, upstream_model: "us.anthropic.claude-3-7-sonnet-20250219-v1:0" })).toEqual({});
    // Other connections keep free-form upstream names.
    expect(validateFields(fields, { ...values, provider_connection_id: provider.id, upstream_model: "anything goes?" })).toEqual({});
    expect(upstream.helpFor!({ ...values, provider_connection_id: provider.id })).toBeUndefined();
  });
});
