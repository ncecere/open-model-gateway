/*
 * Admin › Settings › Sign-in: the OIDC configuration the server started with,
 * read-only (it comes from the environment and changes with a restart). Never
 * the client secret. Group-to-role mappings stay on Admin › SSO groups.
 */
import type { ReactNode } from "react";
import { ArrowRight } from "lucide-react";
import type { Session } from "../../lib/api";
import { settingsPath, type SignInSettings } from "../../lib/settings";
import { Badge, Button, ErrorNotice, Stack, useApi } from "../../components/ui";
import { ResourceLink } from "../../components/navigation-link";
import { Card } from "../../components/ui/card/card";
import { CopyField } from "../../components/ui/copy-field/copy-field";
import { DescriptionList } from "../../components/ui/description-list/description-list";
import { SettingsPage } from "./shared";
import st from "./settings.module.css";

const code = (value?: string) => value ? <code className={st.code}>{value}</code> : "—";

export function SignInSettingsPage(_: { session: Session }) {
  const q = useApi<SignInSettings>(`${settingsPath}/sign-in`);
  const page = (body: ReactNode) => <SettingsPage title="Sign-in" description="How people sign in with single sign-on. Set in the server environment; shown here for reference.">{body}</SettingsPage>;
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
        ]} />
        {c.callback_url && <CopyField label="Callback URL" name="callback URL" value={c.callback_url} description="Register this redirect URI with your identity provider." />}
      </Stack> : <DescriptionList dividers items={[{ label: "Status", value: <Badge>Not configured</Badge> }, { label: "Effect", value: "Nobody can sign in to the dashboard until OIDC is set up." }]} />}
    </Card>
    <Card title="SSO groups" description="Signing in alone gives no access. Group mappings and manual grants give people their roles." actions={<Button variant="secondary" size="sm" render={<ResourceLink search={{ page: "oidc" }} />}>SSO groups <ArrowRight aria-hidden /></Button>}>
      <DescriptionList dividers items={[{ label: "Enabled group mappings", value: String(c.enabled_group_mappings) }]} />
    </Card>
  </>);
}
