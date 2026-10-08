/*
 * Admin › Settings building blocks (Grounded settings pages: a page header,
 * one card per section, a sticky SaveBar while there are edits). Auditors get
 * description lists instead of disabled forms (review rule 12).
 */
import type { ReactNode } from "react";
import { Badge, Button, Heading, Stack } from "../../components/ui";
import { NavigationGuard } from "../../components/navigation-guard";
import { StickySaveBar } from "../../components/templates/sticky-save-bar";
import s from "../shared.module.css";
import st from "./settings.module.css";

export const readOnlyNote = "Only Platform Admins can change these settings.";

export function SettingsPage({ title, description, children }: { title: string; description: ReactNode; children: ReactNode }) {
  return <Stack gap={6} className={s.page}><Heading title={title} description={description} />{children}</Stack>;
}

/** Discard/Save in the sticky bar, plus the leave-page guard, for Platform Admins only. */
export function SaveControls({ writable, dirty, invalid, busy, saveLabel, onSave, onDiscard }: { writable: boolean; dirty: boolean; invalid: boolean; busy: boolean; saveLabel: string; onSave: () => void; onDiscard: () => void }) {
  if (!writable) return null;
  return <>
    <StickySaveBar open={dirty} message={invalid ? "Not saved: fix the highlighted field" : "Unsaved changes"}>
      <Button variant="secondary" disabled={busy} onClick={onDiscard}>Discard</Button>
      <Button loading={busy} onClick={onSave}>{saveLabel}</Button>
    </StickySaveBar>
    <NavigationGuard dirty={dirty && !busy} />
  </>;
}

/** A value the server environment sets: shown, never edited here. */
export function EnvironmentLock({ variable }: { variable: string }) {
  return <span className={st.locked}><Badge>Set by the environment</Badge><code className={st.code}>{variable}</code></span>;
}
