/*
 * Admin › Settings › Admin API tokens (#37 PR3, approved issue comment 6362968687). Long-lived
 * management credentials a Platform Admin can use from scripts and automation: five explicit
 * write-does-not-imply-read scopes plus platform:read, an optional CIDR allowlist, and a 1–90
 * day expiry (default 30; no renewal). They can never call inference or issue inference keys.
 * Token CRUD is browser-session-only; the backend denies these routes to admin tokens
 * themselves. A caller creates only for themself; any Admin may revoke any token, idempotently.
 *
 * This page (and its Settings nav entry, in lib/navigation.ts) requires platform_write, not the
 * platform_read that gates the rest of Admin: see the SPECIAL case in lib/permissions.ts canView,
 * checked before the generic platformPages rule, so an Auditor never sees or fetches this list.
 *
 * Creation discloses the plaintext token once, following pages/keys.tsx CreateKeyDialog's
 * lifetime-safe pattern: ephemeral component state only (never useMutation or a query/mutation
 * cache), ended irreversibly on close, unmount, navigation, omg:unauthorized or
 * omg:access-refresh, with every later disclosure/feedback/Copy guarded by that same generation
 * so a retained callback or a late-completing request can never resurrect it. Unlike
 * CreateKeyDialog, the in-flight request's AbortController is aborted on retirement too (admin
 * tokens are more privileged); either way, a late server completion is still ignored rather than
 * disclosed or auto-retried.
 */
import { useId, useLayoutEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Plus } from "lucide-react";
import { api, platformPath, type Session } from "../lib/api";
import {
  adminTokenAllowlistSummary, adminTokenCidrError, adminTokenExpiryDefault, adminTokenExpiryError,
  adminTokenNameError, adminTokenPill, adminTokenScopes, adminTokenScopesError, adminTokenStatus,
  canRevokeAdminToken, parseCidrAllowlist, scopeLabel, type AdminToken, type AdminTokenScope,
} from "../lib/admin-tokens";
import { Alert, Button, CollectionTable, ErrorNotice, FormField, Heading, Input, Stack, Textarea, useAction } from "../components/ui";
import { ActionMenu } from "../components/templates/action-menu";
import { CopyId } from "../components/templates/copy-id";
import { StatusPill } from "../components/templates/status-pill";
import { PersonIdentity, copyIdAction } from "../components/people";
import { Checkbox, CheckboxGroup } from "../components/ui/checkbox/checkbox";
import { CellText } from "../components/ui/data-table/data-table";
import { Dialog } from "../components/ui/dialog/dialog";
import { Time } from "../components/ui/time/time";
import s from "./shared.module.css";

export const adminTokensPath = `${platformPath}/admin-tokens`;

export function AdminTokensPage({ session }: { session: Session }) {
  const ask = useAction();
  const [creating, setCreating] = useState(false);
  const createTrigger = useRef<HTMLButtonElement>(null);
  const closeCreate = () => { setCreating(false); requestAnimationFrame(() => createTrigger.current?.focus()); };
  const revoke = (token: AdminToken) => ask({
    title: `Revoke "${token.name}"?`,
    description: `${token.owner_display_name?.trim() || token.owner_email}'s token stops working immediately. This can't be undone: a revoked token can never be re-enabled.`,
    danger: true, submitLabel: "Revoke token", successNotice: "Token revoked.",
    run: (_, signal) => api(`${adminTokensPath}/${encodeURIComponent(token.id)}`, { method: "DELETE", signal }),
  });
  return <Stack gap={6} className={s.page}>
    <Heading title="Admin API tokens" description="Long-lived credentials for scripts and automation, scoped to platform management only. They can never call inference or issue inference keys."
      actions={<Button ref={createTrigger} onClick={() => setCreating(true)}><Plus aria-hidden /> Create token</Button>} />
    <CollectionTable<AdminToken> path={adminTokensPath} label="Admin API tokens" pageSize={100} empty="Create a token to call platform management routes from a script." rowKey={t => t.id} columns={[
      { title: "Name", render: t => <span>{t.name}<span className={s.secondary}><CopyId value={t.id} label="token ID" /></span></span> },
      { title: "Owner", render: t => <PersonIdentity person={{ display_name: t.owner_display_name, email: t.owner_email }} self={t.owner_user_id === session.user.id} /> },
      { title: "Status", render: t => <StatusPill {...adminTokenPill(adminTokenStatus(t))} explain /> },
      { title: "Scopes", render: t => <span title={t.scopes.join(", ")}>{t.scopes.map(scopeLabel).join(", ")}</span> },
      { title: "Expires", render: t => <Time value={t.expires_at} format="date" /> },
      { title: "Last used", render: t => t.last_used_at ? <CellText primary={<Time value={t.last_used_at} format="relative" />} secondary={t.last_used_address ?? undefined} /> : <span className={s.muted}>Never</span>, narrow: true },
      // Immutable metadata, set at creation and never edited: hidden by default (Columns menu), never a control.
      { title: "IP allowlist", render: t => <span title={t.ip_allowlist?.join(", ")}>{adminTokenAllowlistSummary(t.ip_allowlist)}</span>, hidden: true },
      { title: "Created", render: t => <Time value={t.created_at} format="date" />, hidden: true },
      { title: "Actions", render: t => <ActionMenu label={`Actions for ${t.name}`} actions={[
        copyIdAction(t.id, "Copy token ID"),
        { label: "Revoke token…", danger: true, hidden: !canRevokeAdminToken(adminTokenStatus(t)), onSelect: () => revoke(t) },
      ]} /> },
    ]} />
    {creating && <CreateAdminTokenDialog session={session} onClose={closeCreate} />}
  </Stack>;
}

type CreateResult = { id: string; token: string; scopes: AdminTokenScope[]; created_at: string; expires_at: string };

/**
 * Create admin token: name, scopes (at least one; platform:read preselected), expiry (1–90
 * days, default 30) and an optional CIDR allowlist, one entry per line or comma-separated
 * (parseCidrAllowlist does no network validation itself; the server is authoritative). Plaintext
 * is shown once and never cached, persisted or auto-retried.
 */
export function CreateAdminTokenDialog({ session, onClose }: { session: Session; onClose: (navigated?: boolean) => void }) {
  const client = useQueryClient(), formId = useId();
  const [name, setName] = useState(""), [scopes, setScopes] = useState<string[]>(["platform:read"]);
  const [expiry, setExpiry] = useState(String(adminTokenExpiryDefault)), [cidr, setCidr] = useState("");
  const [submitted, setSubmitted] = useState(false), [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  const [result, setResult] = useState<CreateResult>(), [copied, setCopied] = useState(false), [copyFailed, setCopyFailed] = useState(false);
  // Copy bindings retain only this lifetime's authority, never a later disclosure's.
  const copyGeneration = useRef(0), displayedGeneration = copyGeneration.current;
  // Mount replay may revive this same owner (StrictMode), but never undo its retirement.
  const lifetime = useRef({ mounted: false, closed: false });
  const controller = useRef<AbortController | undefined>(undefined);
  // Synchronous guards, checked and set before any await: a busy React *state* read is stale
  // for a second submit dispatched in the same event-loop turn (both see busy===false), which
  // would POST two high-privilege tokens from one connected form. `succeeded` additionally seals
  // creation forever after a first success, so a retained old submit binding can never mint a
  // second credential even though it stays directly callable.
  const inFlight = useRef(false), succeeded = useRef(false);
  useLayoutEffect(() => {
    lifetime.current.mounted = true;
    // Home's checking/removal render is asynchronous; retire before dispatch returns.
    const retire = () => { lifetime.current.closed = true; copyGeneration.current++; controller.current?.abort(); };
    window.addEventListener("omg:unauthorized", retire);
    window.addEventListener("omg:access-refresh", retire);
    return () => {
      window.removeEventListener("omg:unauthorized", retire);
      window.removeEventListener("omg:access-refresh", retire);
      lifetime.current.mounted = false; copyGeneration.current++; controller.current?.abort();
    };
  }, []);
  const errors = {
    name: adminTokenNameError(name),
    scopes: adminTokenScopesError(scopes),
    expiry: adminTokenExpiryError(expiry),
    cidr: adminTokenCidrError(cidr),
  };
  const invalid = Object.values(errors).some(Boolean);
  const dirty = !result && (!!name || cidr.trim() !== "" || expiry.trim() !== String(adminTokenExpiryDefault) || scopes.length !== 1 || scopes[0] !== "platform:read");
  const shown = (message?: string) => submitted ? message : undefined;
  async function submit() {
    if (!lifetime.current.mounted || lifetime.current.closed) return;
    setSubmitted(true);
    if (invalid || inFlight.current || succeeded.current) return;
    inFlight.current = true;
    const generation = copyGeneration.current;
    const request = new AbortController(); controller.current = request;
    setBusy(true); setError(undefined);
    try {
      const allowlist = parseCidrAllowlist(cidr);
      const created = await api<CreateResult>(adminTokensPath, { method: "POST", body: { name: name.trim(), scopes, expires_in_days: Number(expiry.trim()), ...(allowlist ? { ip_allowlist: allowlist } : {}) }, signal: request.signal });
      // Validate the current generation AFTER the await, BEFORE disclosure: retirement during
      // the request (close, unmount, navigation, a role/authority change) must not resurrect it.
      if (!lifetime.current.mounted || lifetime.current.closed || copyGeneration.current !== generation) return;
      if (typeof created?.token !== "string" || !created.token) throw new Error("No token was returned. Check the list before trying again.");
      succeeded.current = true;
      void client.invalidateQueries({ queryKey: ["api"] });
      setResult(created);
    } catch (caught) {
      if (request.signal.aborted || copyGeneration.current !== generation) return;
      setError(caught);
    } finally { inFlight.current = false; if (copyGeneration.current === generation) setBusy(false); }
  }
  async function copySecret() {
    if (lifetime.current.closed || copyGeneration.current !== displayedGeneration || !result) return;
    setCopyFailed(false);
    try {
      await navigator.clipboard.writeText(result.token);
      if (copyGeneration.current !== displayedGeneration) return;
      setCopied(true);
    } catch { if (copyGeneration.current === displayedGeneration) setCopyFailed(true); }
  }
  const close = (navigated = false) => { if (!busy) { lifetime.current.closed = true; copyGeneration.current++; controller.current?.abort(); setResult(undefined); onClose(navigated); } };
  if (result) return <Dialog open title="Save your admin API token" description="Use it from your script or automation. It can't sign in to this dashboard." onOpenChange={open => { if (!open) close(); }}
    footer={<Button onClick={() => close()}>I have saved it</Button>}>
    <Stack gap={4}>
      <Alert tone="warning" title="Shown only once">Store this token securely. Closing this dialog clears it from the page.</Alert>
      <p className={s.note}>Scopes: {result.scopes.map(scopeLabel).join(", ")}.</p>
      <FormField label="Admin API token"><Textarea readOnly value={result.token} rows={3} spellCheck={false} autoComplete="off" autoFocus onFocus={event => event.currentTarget.select()} /></FormField>
      <Button variant="secondary" onClick={copySecret}>{copied ? "Copied" : "Copy token"}</Button>
      {/* Never the secret itself: a visible manual fallback, same pattern as the generic action dialog's one-time secret. */}
      <p role="status">{copyFailed ? "Clipboard unavailable. Select and copy the token manually." : ""}</p>
    </Stack>
  </Dialog>;
  return <Dialog open title="Create admin API token" description="A long-lived credential scoped to platform management routes only." hideClose={busy} onOpenChange={open => { if (!open) close(); }}
    footer={<><Button variant="secondary" disabled={busy} onClick={() => close()}>Cancel</Button><Button type="submit" form={formId} loading={busy}>Create token</Button></>}>
    <form id={formId} noValidate aria-busy={busy} data-dirty={dirty ? "true" : undefined} onSubmit={event => { event.preventDefault(); void submit(); }}>
      <Stack gap={5}>
        <FormField label="Name" error={shown(errors.name)}><Input value={name} maxLength={120} autoFocus autoComplete="off" disabled={busy} onChange={event => setName(event.target.value)} /></FormField>
        <CheckboxGroup legend="Scopes" description="Write scopes do not grant read access. Access and settings scopes are highly privileged: they can manage users, roles, workspaces and installation settings. No scope lets this token call inference or issue inference keys." error={shown(errors.scopes)} value={scopes} onValueChange={setScopes} disabled={busy}>
          {adminTokenScopes.map(scope => <Checkbox key={scope.value} value={scope.value} label={scope.label} description={scope.description} />)}
        </CheckboxGroup>
        <FormField label="Expires in (days)" description="1 to 90 days. There's no renewal: create a new token when this one expires." error={shown(errors.expiry)}><Input value={expiry} inputMode="numeric" maxLength={2} disabled={busy} onChange={event => setExpiry(event.target.value)} /></FormField>
        <FormField label="IP allowlist" labelHint="Optional" description="One CIDR per line or comma-separated. Leave blank for unrestricted access from any address." error={shown(errors.cidr)}><Textarea value={cidr} rows={3} spellCheck={false} autoComplete="off" disabled={busy} placeholder="203.0.113.0/24" onChange={event => setCidr(event.target.value)} /></FormField>
        {error !== undefined && <ErrorNotice error={error} />}
      </Stack>
    </form>
  </Dialog>;
}
