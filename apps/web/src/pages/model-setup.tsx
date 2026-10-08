import { useEffect, useId, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { ApiError, api, platformPath, type Catalog, type ModelSetupResult, type Provider, type Session } from "../lib/api";
import { validateFields, type Values } from "../lib/forms";
import { apiNameFrom, chosenConnection, defaultProtocols, initialSetupValues, protocolProfiles, selectedIds, setupAvailabilityFields, setupBody, setupFields, setupIdentityFields, setupSourceFields, workloadGroups, workloadSupported, type SetupChoices } from "../lib/model-setup";
import type { WorkloadKind } from "../lib/governance";
import { draftBody, emptyDraft, validateDraft, workloadLabels, type PriceDraft } from "../lib/pricing";
import { PriceLinesEditor, focusFirstPriceError } from "../components/price-editor";
import { Checkbox, CheckboxGroup } from "../components/ui/checkbox/checkbox";
import { Alert, Heading, Stack, useChoices } from "../components/ui";
import { FieldControls, FormPage, FormSection, Wide } from "../components/templates/form-page";
import { RadioGroup } from "../components/ui/radio-group/radio-group";
import { Switch } from "../components/ui/switch/switch";
import { toast } from "../components/ui/toast/toast";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { providerLabel } from "./catalog";
import { ProviderIcon, WithIcon } from "../components/provider-icon";
import s from "./shared.module.css";

/** 409 from model-setup is either a taken API name or a connection that no longer exists; the gateway does not say which. */
export const conflictErrors = { public_name: "This API model name may already be in use. Choose another.", provider_connection_id: "Or this connection no longer exists. Choose it again from the refreshed list." };

/** Admin › Models › Add model (/admin/models/new?connection=): connection → model in one request. */
export function AddModel({ session, connection }: { session: Session; connection?: string }) {
  if (!session.capabilities.platform_write) return <Stack gap={6} className={s.page}><Heading title="Access not available" description="Adding models requires Platform Admin. Auditors can review models but not change them." /><ResourceLink search={{ page: "models" }}>Back to Models</ResourceLink></Stack>;
  return <AddModelForm connection={connection} />;
}

function AddModelForm({ connection }: { connection?: string }) {
  const nav = useDashboardNavigation(), client = useQueryClient(), id = useId();
  const connections = useChoices<Provider>(`${platformPath}/providers`), catalogs = useChoices<Catalog>(`${platformPath}/catalogs`);
  const [values, setValues] = useState<Values>(initialSetupValues), initial = useRef(values);
  const [price, setPrice] = useState<PriceDraft>(() => emptyDraft("generation")), initialPrice = useRef(price), [priceErrors, setPriceErrors] = useState<Record<string, string>>({});
  const [errors, setErrors] = useState<Record<string, string>>({}), [error, setError] = useState<unknown>(), [busy, setBusy] = useState(false);
  // Workload is a single choice (radio cards); protocols follow the connection's profile until a text protocol is picked by hand.
  const [workload, setWorkload] = useState<WorkloadKind>("generation"), [protocolsTouched, setProtocolsTouched] = useState(false);
  const completed = useRef(false), inFlight = useRef(false), controller = useRef<AbortController | undefined>(undefined);
  useEffect(() => () => controller.current?.abort(), []);
  const choices: SetupChoices = { connections: connections.data ?? [], catalogs: catalogs.data ?? [] };
  // The connection is derived until chosen, so ?connection= works before options load (Grounded chosenConnection).
  const connectionId = chosenConnection(values.provider_connection_id, choices.connections, connection), profile = choices.connections.find(c => c.id === connectionId)?.provider;
  const current: Values = { ...values, provider_connection_id: connectionId, supported_protocols: protocolsTouched ? values.supported_protocols : JSON.stringify(defaultProtocols(workload, profile)) };
  const baselineConnection = chosenConnection("", choices.connections, connection);
  const baseline = { ...initial.current, provider_connection_id: baselineConnection, supported_protocols: JSON.stringify(defaultProtocols("generation", choices.connections.find(c => c.id === baselineConnection)?.provider)) };
  const dirty = !completed.current && (JSON.stringify(current) !== JSON.stringify(baseline) || (current.pricing === "priced" && price !== initialPrice.current));
  // Clients should send the provider's own ID ("claude-haiku-5-5"), not a slug of the display name ("claude-haiku-5.5").
  const followName = (prev: Values) => prev.public_name === "" || prev.public_name === apiNameFrom(prev.upstream_model);

  const change = (name: string, value: string) => {
    if (busy || current[name] === value) return;
    setValues(prev => {
      const next = { ...prev, provider_connection_id: current.provider_connection_id, supported_protocols: current.supported_protocols, [name]: value };
      // The API name follows the upstream model ID until edited; the display name is free text.
      if (name === "upstream_model" && followName(prev)) next.public_name = apiNameFrom(next.upstream_model);
      return next;
    });
    if (name === "supported_protocols") setProtocolsTouched(true);
    // Validation can depend on other fields; release controlled invalid state so the next submit revalidates.
    setErrors({}); setPriceErrors({});
  };
  const changePrice = (next: PriceDraft) => { if (busy) return; setPrice(next); setPriceErrors({}); };
  // Each workload has its own meters: a new workload starts a fresh, all-Unknown price draft and its default protocol.
  const changeWorkload = (next: WorkloadKind) => { if (busy || next === workload) return; setWorkload(next); setProtocolsTouched(false); if (next !== price.workload) setPrice(emptyDraft(next)); setErrors({}); setPriceErrors({}); };

  async function submit() {
    if (inFlight.current || !connections.data || !catalogs.data) return;
    const fields = setupFields(choices, current), validation = validateFields(fields, current), priceValidation = current.pricing === "priced" ? validateDraft(price) : {};
    setErrors(validation); setPriceErrors(priceValidation);
    if (Object.keys(validation).length) { document.getElementById(`${id}-${Object.keys(validation)[0]}`)?.focus(); return; }
    if (Object.keys(priceValidation).length) { focusFirstPriceError(`${id}-price`, priceValidation); return; }
    let body; try { body = setupBody(current, choices, current.pricing === "priced" ? draftBody(price) : null); } catch (caught) { setError(caught); return; }
    const request = new AbortController(); controller.current = request; inFlight.current = true; setBusy(true); setError(undefined);
    try {
      const result = await api<ModelSetupResult>(`${platformPath}/model-setup`, { method: "POST", body, signal: request.signal });
      if (request.signal.aborted) return;
      completed.current = true;
      void client.invalidateQueries({ queryKey: ["api"] });
      toast.success(`${body.model.display_name} added.`);
      nav?.navigate({ page: "model-detail", record: result.model_id });
    } catch (caught) {
      if (request.signal.aborted) return;
      if (caught instanceof ApiError && caught.status === 409) { setErrors(conflictErrors); void connections.refetch(); document.getElementById(`${id}-public_name`)?.focus(); }
      else { setError(caught); if (caught instanceof ApiError && caught.status === 403) void client.invalidateQueries({ queryKey: ["session"] }); }
    } finally { inFlight.current = false; if (!request.signal.aborted) setBusy(false); }
  }

  const loadError = connections.error ?? catalogs.error;
  const noConnections = connections.data?.length === 0;
  const priced = current.pricing === "priced", enabled = current.enabled === "true";
  const selected = choices.connections.find(c => c.id === current.provider_connection_id);
  return <FormPage label="Add model" title="Add model" description="Offer a model from a connection. The model, its first route, an optional price and its catalogs are created together, or not at all." back={{ label: "Models", search: { page: "models" } }}
    onCancel={() => nav?.navigate({ page: "models" })} onSubmit={() => void submit()} submitLabel="Add model" busy={busy} dirty={dirty} allowLeave={() => completed.current} error={loadError ?? error}
    loading={!connections.data || !catalogs.data ? (loadError ? "Options could not be loaded." : "Loading connections and catalogs…") : undefined}>
    {noConnections && <Alert tone="warning" title="No connections yet">Add a connection first: <ResourceLink search={{ page: "providers" }}>Connections</ResourceLink>.</Alert>}
    <FormSection title="Source" description={selected ? <><WithIcon icon={<ProviderIcon profile={selected.provider} />}>{providerLabel(selected.provider ?? "")}</WithIcon>{selected.enabled === false ? " · this connection is disabled, so the route will not serve until it is enabled" : ""}</> : undefined}><FieldControls fields={setupSourceFields(choices, profile, workload).slice(0, 2)} values={current} errors={errors} disabled={busy} idPrefix={id} onChange={change} /><WorkloadPicker value={workload} profile={profile} disabled={busy} onChange={changeWorkload} />{workload === "generation" && <ProtocolPicker id={`${id}-supported_protocols`} value={current.supported_protocols} profile={profile} error={errors.supported_protocols} disabled={busy} onChange={value => change("supported_protocols", value)} />}{workload !== "generation" && errors.supported_protocols && <Wide><p id={`${id}-supported_protocols`} tabIndex={-1} className={s.dangerText}>{errors.supported_protocols}</p></Wide>}</FormSection>
    <FormSection title="Identity"><FieldControls fields={setupIdentityFields()} values={current} errors={errors} disabled={busy} idPrefix={id} onChange={change} /></FormSection>
    <FormSection title="Availability">{choices.catalogs.length ? <FieldControls fields={setupAvailabilityFields(choices)} values={current} errors={errors} disabled={busy} idPrefix={id} onChange={change} /> : <Wide><p className={s.note}>No catalogs yet. The model stays unoffered until you add it to a catalog or assign it directly to a workspace.</p></Wide>}</FormSection>
    <FormSection title="Pricing">
      <Wide><RadioGroup legend="Price for this route" value={current.pricing} disabled={busy} onValueChange={value => change("pricing", value)} options={[{ value: "unpriced", label: "Leave unpriced", description: "Usage is recorded with unknown cost. You can publish a price later from the model page." }, { value: "priced", label: "Set a price now", description: `US dollars per unit for ${workloadLabels[price.workload].toLowerCase()} meters, exact to the micro-dollar. Prices are estimates, not provider invoices.` }]} /></Wide>
      {priced && selected?.provider === "openrouter" && <Wide><p className={s.note}>After adding, use Import current OpenRouter price on the model's Pricing tab to draft this route's price from OpenRouter's public catalog.</p></Wide>}
      {priced && <Wide><PriceLinesEditor draft={price} onChange={changePrice} errors={priceErrors} disabled={busy} idPrefix={`${id}-price`} /></Wide>}
    </FormSection>
    <FormSection title="Status">
      <Wide><Switch label="Enable the model and its route" checked={enabled} disabled={busy} onCheckedChange={checked => change("enabled", String(checked))} description={enabled ? "Workspaces that have this model available can call it as soon as its connection is enabled." : "Off by default. Review routing, pricing and availability on the model page, then enable it there."} /></Wide>
    </FormSection>
  </FormPage>;
}

/** The workload as radio cards (one per model); a workload the connection can't serve says so. */
function WorkloadPicker({ value, profile, disabled, onChange }: { value: WorkloadKind; profile?: string; disabled?: boolean; onChange: (value: WorkloadKind) => void }) {
  return <Wide><RadioGroup<WorkloadKind> legend="Workload" description="What clients use this model for. Each model has one workload; add another model for another workload." variant="card" value={value} disabled={disabled} onValueChange={onChange}
    options={workloadGroups.map(g => ({ value: g.workload, label: g.title, description: <>{g.help}{!workloadSupported(g.workload, profile) && <><br /><span className={s.dangerText}>This connection can't serve it.</span></>}</> }))} /></Wide>;
}
/** Text generation only: which client protocols the model answers. Chat Completions, Responses and Messages combine. */
function ProtocolPicker({ id, value, profile, error, disabled, onChange }: { id: string; value: string; profile?: string; error?: string; disabled?: boolean; onChange: (value: string) => void }) {
  const selected = selectedIds(value), text = workloadGroups.find(g => g.workload === "generation")!.protocols;
  return <Wide><CheckboxGroup legend="Client protocols" description="Choose one or more. Serving also requires the connection to support the protocol." error={error} id={id} tabIndex={-1} value={selected} disabled={disabled} onValueChange={next => onChange(JSON.stringify(text.map(p => p.value).filter(p => next.includes(p))))}>
    {text.map(o => <Checkbox key={o.value} value={o.value} label={o.label} description={profile && !protocolProfiles[o.value].includes(profile) ? "Not supported by this connection" : undefined} />)}
  </CheckboxGroup></Wide>;
}
