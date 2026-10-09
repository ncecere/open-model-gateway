/*
 * Admin › Settings › Sign-in: the OIDC configuration the server started with,
 * read-only (it comes from the environment and changes with a restart). Never
 * the client secret or the SCIM token. Group-to-role mappings stay on Admin › SSO groups.
 */
import type { ReactNode } from "react";
import { ArrowRight } from "lucide-react";
import type { Session } from "../../lib/api";
import { settingsPath, type JwksStatus, type ScimStatus, type SignInSettings } from "../../lib/settings";
import { Badge, Button, ErrorNotice, Stack, useApi } from "../../components/ui";
import { ResourceLink } from "../../components/navigation-link";
import { Card } from "../../components/ui/card/card";
import { CopyField } from "../../components/ui/copy-field/copy-field";
import { DescriptionList } from "../../components/ui/description-list/description-list";
import { Time } from "../../components/ui/time/time";
import { SettingsPage } from "./shared";
import st from "./settings.module.css";

const code = (value?: string) => value ? <code className={st.code}>{value}</code> : "—";
const plural = (n: number, word: string) => `${n.toLocaleString()} ${word}${n === 1 ? "" : "s"}`;

function SigningKeys({ jwks }: { jwks: JwksStatus }) {
  const badge = jwks.state === "fresh" ? <Badge tone="good">Current</Badge> : jwks.state === "stale" ? <Badge>Cached copy</Badge> : <Badge tone="bad">Unavailable</Badge>;
  return <span title={jwks.last_failure_at ? `Last refresh failed ${new Date(jwks.last_failure_at).toLocaleString()}` : undefined}>
    {badge} {plural(jwks.keys, "key")} · refreshed <Time value={jwks.refreshed_at} format="relative" />
  </span>;
}

function Provisioning({ scim }: { scim: ScimStatus }) {
  if (!scim.enabled) return <DescriptionList dividers items={[
    { label: "Status", value: <Badge>Off</Badge> },
    { label: "Turn on", value: <>Set <code className={st.code}>GATEWAY_SCIM_TOKEN_ENV</code></> },
  ]} />;
  return <Stack gap={5}>
    <DescriptionList dividers items={[
      { label: "Status", value: <Badge tone="good">On</Badge> },
      { label: "Users", value: `${scim.active_users.toLocaleString()} active of ${scim.users.toLocaleString()}` },
      { label: "Groups", value: `${plural(scim.groups, "group")} · ${plural(scim.memberships, "membership")}` },
      { label: "Last sync", value: scim.last_sync_at ? <Time value={scim.last_sync_at} format="relative" /> : "Never" },
    ]} />
    <CopyField label="SCIM base URL" name="SCIM base URL" value={scim.base_url} description="Use with the bearer token named by GATEWAY_SCIM_TOKEN_ENV." />
  </Stack>;
}

export function SignInSettingsPage(_: { session: Session }) {
  const q = useApi<SignInSettings>(`${settingsPath}/sign-in`);
  const page = (body: ReactNode) => <SettingsPage title="Sign-in" description="Single sign-on, set in the server environment.">{body}</SettingsPage>;
  if (q.isError) return page(<ErrorNotice error={q.error} retry={() => void q.refetch()} />);
  if (!q.data) return page(<p role="status">Loading settings…</p>);
  const c = q.data;
  return page(<>
    <Card title="Single sign-on (OIDC)" description={<>From <code>GATEWAY_OIDC_ISSUER</code>, <code>GATEWAY_OIDC_CLIENT_ID</code>, <code>GATEWAY_OIDC_GROUPS_CLAIM</code> and <code>GATEWAY_PUBLIC_URL</code>. A change applies after a restart. The client secret is never shown.</>}>
      {c.enabled ? <Stack gap={5}>
        <DescriptionList dividers items={[
          { label: "Status", value: <Badge tone="good">Configured</Badge> },
          { label: "Issuer", value: code(c.issuer) },
          { label: "Client ID", value: code(c.client_id) },
          { label: "Client type", value: c.client_type === "confidential" ? "Confidential (with a client secret)" : "Public (PKCE only)" },
          { label: "Groups claim", value: code(c.groups_claim) },
          { label: "Gateway address", value: code(c.public_url) },
          ...(c.jwks ? [{ label: "Signing keys", value: <SigningKeys jwks={c.jwks} /> }] : []),
        ]} />
        {c.callback_url && <CopyField label="Callback URL" name="callback URL" value={c.callback_url} description="Register this redirect URI with your identity provider." />}
      </Stack> : <DescriptionList dividers items={[{ label: "Status", value: <Badge>Not configured</Badge> }, { label: "Effect", value: "Nobody can sign in to the dashboard until OIDC is set up." }]} />}
    </Card>
    <Card title="Provisioning (SCIM)" description="Users and groups pushed by your identity provider.">
      <Provisioning scim={c.scim ?? { enabled: false }} />
    </Card>
    <Card title="SSO groups" description="Roles come from group mappings or manual grants." actions={<Button variant="secondary" size="sm" render={<ResourceLink search={{ page: "oidc" }} />}>SSO groups <ArrowRight aria-hidden /></Button>}>
      <DescriptionList dividers items={[{ label: "Enabled group mappings", value: String(c.enabled_group_mappings) }]} />
    </Card>
  </>);
}
