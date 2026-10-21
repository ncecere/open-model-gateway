/*
 * "Your data" on Your profile (#74): download what the gateway holds about
 * you as one JSON file. Request records and your audit entries cover a
 * chosen range of up to 93 days; files are listed, not included (download
 * them while they exist); key secrets, prompts and responses are never in it.
 */
import { useState } from "react";
import { Download } from "lucide-react";
import { API } from "../lib/api";
import { downloadExport, MAX_EXPORT_DAYS, rangeDays } from "../lib/subject-export";
import { Button, ErrorNotice, FormField, Inline, Input, Stack } from "./ui";
import { Card } from "./ui/card/card";

const day = (d: Date) => d.toISOString().slice(0, 10);

export function YourData() {
  const today = day(new Date());
  const [from, setFrom] = useState(day(new Date(Date.now() - 29 * 86_400_000))), [to, setTo] = useState(today);
  const [busy, setBusy] = useState(false), [error, setError] = useState<unknown>();
  const days = rangeDays(from, to);
  const invalid = !(days >= 1 && days <= MAX_EXPORT_DAYS) || to > today;
  async function run() {
    setBusy(true); setError(undefined);
    try { await downloadExport(`${API}/me/export?from=${from}&to=${to}`, `omg-export-${from}-${to}.json`); }
    catch (e) { setError(e); } finally { setBusy(false); }
  }
  return <Card title="Your data" titleAs="h2" description="Download your profile, memberships, key details, requests, file list and audit entries as JSON. Requests and audit entries cover the dates you choose (up to 93 days).">
    <Stack gap={4} align="start">
      <Inline gap={4} align="start">
        <FormField label="From"><Input type="date" value={from} max={to} onChange={e => setFrom(e.target.value)} /></FormField>
        <FormField label="To" error={invalid ? `Choose up to ${MAX_EXPORT_DAYS} days, ending today or earlier.` : undefined}><Input type="date" value={to} max={today} onChange={e => setTo(e.target.value)} /></FormField>
      </Inline>
      {error !== undefined && <ErrorNotice error={error} />}
      <Button variant="secondary" loading={busy} disabled={invalid} onClick={() => void run()}><Download aria-hidden /> Download your data</Button>
    </Stack>
  </Card>;
}
