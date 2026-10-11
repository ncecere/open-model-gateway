import { Fragment, useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { AudioLines, Binary, BrainCircuit, Clapperboard, Headphones, Image, Layers, ListOrdered, MessageSquareText, Mic } from "lucide-react";
import { ApiError, api, platformPath, type Catalog, type ModelProtocol, type ModelSetupResult, type Provider, type Session } from "../lib/api";
import { validateFields, type Values } from "../lib/forms";
import { apiNameFrom, chosenConnection, defaultProtocols, initialSetupValues, protocolSupported, selectedIds, setupBody, setupFields, setupIdentityFields, setupSourceFields, workloadDisabledReason, workloadGroups, workloadSupported, type SetupChoices } from "../lib/model-setup";
import type { Meter, WorkloadKind } from "../lib/governance";
import { METER_SPECS, draftBody, emptyDraft, validateDraft, type PriceDraft } from "../lib/pricing";
import { PriceLinesEditor, priceFieldId } from "../components/price-editor";
import { Alert, FormField, Heading, Input, NativeSelect, Stack, useChoices } from "../components/ui";
import { FieldControls, FormPage, FormSection } from "../components/templates/form-page";
import { IconSelect } from "../components/icon-select";
import { Disclosure } from "../components/ui/disclosure/disclosure";
import { Switch } from "../components/ui/switch/switch";
import { ToggleGroup, ToggleGroupItem } from "../components/ui/toggle-group/toggle-group";
import { toast } from "../components/ui/toast/toast";
import { ResourceLink, useDashboardNavigation } from "../components/navigation-link";
import { providerLabel } from "./catalog";
import { ProviderIcon } from "../components/provider-icon";
import s from "./shared.module.css";
import m from "./model-setup.module.css";

/** 409 from model-setup is either a taken API name or a connection that no longer exists; the gateway does not say which. */
export const conflictErrors = { public_name: "This name may already be taken. Choose another.", provider_connection_id: "Or this connection was removed. Choose it again." };

/** Type icons (decorative; the label is the name). */
const workloadIcons: Record<WorkloadKind, ReactNode> = { generation: <MessageSquareText />, embeddings: <Binary />, images: <Image />, audio_transcriptions: <Mic />, audio_speech: <AudioLines />, rerank: <ListOrdered />, systemone: <BrainCircuit />, realtime: <Headphones />, videos: <Clapperboard />, batches: <Layers /> };

/** Admin › Models › Add model (/admin/models/new?connection=): connection → model in one request. */
export function AddModel({ session, connection }: { session: Session; connection?: string }) {
  if (!session.capabilities.platform_write) return <Stack gap={6} className={s.page}><Heading title="Access not available" description="Only Platform Admins can add models." /><ResourceLink search={{ page: "models" }}>Back to Models</ResourceLink></Stack>;
  return <AddModelForm connection={connection} />;
}

function focusInvalidField(id: string) {
  const field = document.getElementById(id);
  if (!field || field.matches(":disabled")) return false;
  field.focus({ preventScroll: true });
  field.scrollIntoView?.({ block: "center", behavior: "auto" });
  return document.activeElement === field;
}

function priceErrorLabel(key: string) {
  if (key === "limits.input") return "Input token ceiling";
  if (key === "limits.output") return "Output token ceiling";
  const parts = key.split("."), meter = METER_SPECS[parts[0] as Meter];
  const partLabels: Record<string, string> = { usd: "price", batch: "batch price", variant: "variant", tier: "prompt-size tier", rows: "base price", max: "ceiling" };
  const part = partLabels[parts.at(-1)!];
  return meter ? `${meter.title}${part ? ` ${part}` : ""}` : "Pricing";
}

function AddModelForm({ connection }: { connection?: string }) {
  const nav = useDashboardNavigation(), client = useQueryClient(), id = useId();
  const connections = useChoices<Provider>(`${platformPath}/providers`), catalogs = useChoices<Catalog>(`${platformPath}/catalogs`);
  const [values, setValues] = useState<Values>(initialSetupValues), initial = useRef(values);
  const [price, setPrice] = useState<PriceDraft>(() => emptyDraft("generation")), initialPrice = useRef(price), [priceErrors, setPriceErrors] = useState<Record<string, string>>({});
  const [errors, setErrors] = useState<Record<string, string>>({}), [error, setError] = useState<unknown>(), [busy, setBusy] = useState(false);
  const pendingFocus = useRef<string[] | null>(null), validationSummary = useRef<HTMLDivElement>(null);
  // Wait for controlled errors to render before focusing. Editing never schedules a focus jump.
  useLayoutEffect(() => {
    const targets = pendingFocus.current;
    if (!targets) return;
    pendingFocus.current = null;
    if (!targets.some(focusInvalidField)) validationSummary.current?.focus({ preventScroll: true });
  }, [errors, priceErrors]);
  // One type per model; text protocols follow the connection's profile until picked by hand.
  const [workload, setWorkload] = useState<WorkloadKind>("generation"), [protocolsTouched, setProtocolsTouched] = useState(false);
  const completed = useRef(false), inFlight = useRef(false), controller = useRef<AbortController | undefined>(undefined);
  useEffect(() => () => controller.current?.abort(), []);
  const choices: SetupChoices = { connections: connections.data ?? [], catalogs: catalogs.data ?? [] };
  // The connection is derived until chosen, so ?connection= works before options load (Grounded chosenConnection).
  const connectionId = chosenConnection(values.provider_connection_id, choices.connections, connection), profileOf = (cid: string) => choices.connections.find(c => c.id === cid)?.provider, profile = profileOf(connectionId);
  const current: Values = { ...values, provider_connection_id: connectionId, supported_protocols: protocolsTouched ? values.supported_protocols : JSON.stringify(defaultProtocols(workload, profile)) };
  const baselineConnection = chosenConnection("", choices.connections, connection);
  const baseline = { ...initial.current, provider_connection_id: baselineConnection, supported_protocols: JSON.stringify(defaultProtocols("generation", profileOf(baselineConnection))) };
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
  // Each type has its own meters: a new type starts a fresh, all-Unknown price draft and its default protocol.
  const changeWorkload = (next: WorkloadKind) => { if (busy || next === workload) return; setWorkload(next); setProtocolsTouched(false); if (next !== price.workload) setPrice(emptyDraft(next)); setErrors({}); setPriceErrors({}); };
  // Another connection: protocols follow its profile again, and a type it can't serve falls back to Text.
  const changeConnection = (next: string) => {
    if (busy || next === current.provider_connection_id) return;
    change("provider_connection_id", next); setProtocolsTouched(false);
    if (!workloadSupported(workload, profileOf(next))) changeWorkload("generation");
  };

  async function submit() {
    if (inFlight.current || !connections.data || !catalogs.data) return;
    const fields = setupFields(choices, current), validation = validateFields(fields, current), priceValidation = current.pricing === "priced" ? validateDraft(price) : {};
    const targets = [...Object.keys(validation).map(key => `${id}-${key}`), ...Object.keys(priceValidation).map(key => priceFieldId(`${id}-price`, key))];
    pendingFocus.current = targets.length ? targets : null;
    setErrors(validation); setPriceErrors(priceValidation);
    if (targets.length) return;
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
      if (caught instanceof ApiError && caught.status === 409) { pendingFocus.current = [`${id}-public_name`]; setErrors(conflictErrors); void connections.refetch(); }
      else { setError(caught); if (caught instanceof ApiError && caught.status === 403) void client.invalidateQueries({ queryKey: ["session"] }); }
    } finally { inFlight.current = false; if (!request.signal.aborted) setBusy(false); }
  }

  const loadError = connections.error ?? catalogs.error;
  const priced = current.pricing === "priced", enabled = current.enabled === "true";
  const selected = choices.connections.find(c => c.id === current.provider_connection_id);
  const [, upstreamField] = setupSourceFields(choices, profile, workload);
  const providerName = selected?.provider ? providerLabel(selected.provider) : "this connection";
  const fields = setupFields(choices, current);
  const validationIssues = [
    ...Object.keys(errors).map(key => ({ id: `${id}-${key}`, label: fields.find(field => field.name === key)?.label ?? key })),
    ...Object.keys(priceErrors).map(key => ({ id: priceFieldId(`${id}-price`, key), label: priceErrorLabel(key) })),
  ];
  return <FormPage label="Add model" title="Add model" description="Offer a model from one of your connections." back={{ label: "Models", search: { page: "models" } }}
    onCancel={() => nav?.navigate({ page: "models" })} onSubmit={() => void submit()} submitLabel="Add model" busy={busy} dirty={dirty} allowLeave={() => completed.current} error={loadError ?? error}
    loading={!connections.data || !catalogs.data ? (loadError ? "Options could not be loaded." : "Loading…") : undefined}>
    {validationIssues.length > 0 && <Alert ref={validationSummary} tabIndex={-1} tone="danger" aria-label="Form validation" title={`Fix ${validationIssues.length} field${validationIssues.length === 1 ? "" : "s"}`}>
      {validationIssues.map((issue, index) => <Fragment key={issue.id}>{index > 0 && " · "}<a href={`#${issue.id}`} onClick={event => { event.preventDefault(); focusInvalidField(issue.id); }}>{issue.label}</a></Fragment>)}
    </Alert>}
    {connections.data?.length === 0 && <Alert tone="warning" title="No connections yet">Add one first in <ResourceLink search={{ page: "providers" }}>Connections</ResourceLink>.</Alert>}
    <FormSection title="Source">
      <FormField name="provider_connection_id" label="Connection" description={selected?.enabled === false ? "This connection is disabled." : undefined} error={errors.provider_connection_id}>
        <span className={m.leading}>
          <span aria-hidden className={m.leadingIcon}><ProviderIcon profile={selected?.provider} /></span>
          <NativeSelect id={`${id}-provider_connection_id`} name="provider_connection_id" aria-required className={m.withIcon} disabled={busy} value={current.provider_connection_id} onChange={event => changeConnection(event.target.value)}>
            {!current.provider_connection_id && <option value="">Choose…</option>}
            {choices.connections.map(c => <option key={c.id} value={c.id}>{c.name}{c.enabled === false ? " · disabled" : ""}</option>)}
          </NativeSelect>
        </span>
      </FormField>
      <FormField name="upstream_model" label="Upstream model ID" error={errors.upstream_model}>
        <Input id={`${id}-upstream_model`} name="upstream_model" aria-required disabled={busy} value={current.upstream_model} maxLength={upstreamField.maxLength} placeholder={upstreamField.placeholder} autoComplete="off" spellCheck={false} onChange={event => change("upstream_model", event.target.value)} />
      </FormField>
      <IconSelect<WorkloadKind> label="Type" id={`${id}-workload`} value={workload} disabled={busy} onChange={changeWorkload}
        items={workloadGroups.map(g => ({ value: g.workload, label: g.label, icon: workloadIcons[g.workload], disabledReason: workloadDisabledReason(g.workload, profile, providerName) }))} />
      {workload === "generation"
        ? <ProtocolChips id={`${id}-supported_protocols`} value={current.supported_protocols} profile={profile} error={errors.supported_protocols} disabled={busy} onChange={value => change("supported_protocols", value)} />
        : errors.supported_protocols ? <p id={`${id}-supported_protocols`} tabIndex={-1} className={s.dangerText}>{errors.supported_protocols}</p> : <span />}
    </FormSection>
    <FormSection title="Identity"><FieldControls fields={setupIdentityFields(profile, workload)} values={current} errors={errors} disabled={busy} idPrefix={id} onChange={change} /></FormSection>
    <FormSection title="Availability">
      {choices.catalogs.length
        ? <Chips id={`${id}-catalog_ids`} label="Catalogs" value={selectedIds(current.catalog_ids)} error={errors.catalog_ids} disabled={busy} options={choices.catalogs.map(c => ({ value: c.id, label: c.name }))} onChange={next => change("catalog_ids", JSON.stringify(next))} />
        : <div><span className={m.label}>Catalogs</span><p className={s.note}>No catalogs yet.</p></div>}
      <Switch label="Enabled" checked={enabled} disabled={busy} onCheckedChange={checked => change("enabled", String(checked))} description={enabled ? "Callable as soon as its connection is enabled." : "Off until you turn it on."} />
    </FormSection>
    <Disclosure title="Add price now (optional)" open={priced} onOpenChange={open => change("pricing", open ? "priced" : "unpriced")}>
      <Stack gap={3}>
        {selected?.provider === "openrouter" && <p className={s.note}>Or import OpenRouter's price later from the model's Pricing tab.</p>}
        <PriceLinesEditor draft={price} onChange={changePrice} errors={priceErrors} disabled={busy} idPrefix={`${id}-price`} />
      </Stack>
    </Disclosure>
  </FormPage>;
}

/** A compact row of toggle chips for a multiple choice (Bitop ToggleGroup), with an error under it. */
function Chips({ id, label, options, value, error, disabled, onChange }: { id: string; label: string; options: { value: string; label: string }[]; value: string[]; error?: string; disabled?: boolean; onChange: (value: string[]) => void }) {
  const labelId = `${id}-label`, errorId = `${id}-error`;
  return <div className={m.chips}>
    <span id={labelId} className={m.label}>{label}</span>
    <ToggleGroup id={id} tabIndex={-1} multiple variant="outline" size="sm" aria-labelledby={labelId} aria-describedby={error ? errorId : undefined} aria-invalid={error ? true : undefined} disabled={disabled} value={value}
      onValueChange={next => onChange(options.map(o => o.value).filter(v => next.includes(v)))}>
      {options.map(o => <ToggleGroupItem key={o.value} value={o.value}>{o.label}</ToggleGroupItem>)}
    </ToggleGroup>
    {error && <p id={errorId} className={s.dangerText}>{error}</p>}
  </div>;
}

/** Text only: the client protocols the model answers; ones the connection can't serve aren't offered. */
function ProtocolChips({ id, value, profile, error, disabled, onChange }: { id: string; value: string; profile?: string; error?: string; disabled?: boolean; onChange: (value: string) => void }) {
  const text = workloadGroups.find(g => g.workload === "generation")!.protocols.filter(p => protocolSupported(p.value as ModelProtocol, profile));
  return <Chips id={id} label="Protocols" options={text} value={selectedIds(value)} error={error} disabled={disabled} onChange={next => onChange(JSON.stringify(next))} />;
}
