/*
 * RecordPage: one record (a model, a connection) as stacked cards instead of
 * tabs: "Back to …", the title with status badges and header actions
 * (Edit, Disable…), a Details card of key/value facts, then titled section
 * cards. Section content renders its own headings one level down.
 *
 * Provenance: adapted from Grounded web/src/components/templates/record-page.tsx
 * (read-only reference). OMG's version is a routed page with OMG breadcrumbs
 * and canonical links; it has no takeover stack.
 *
 *   <RecordPage title={model.display_name} meta={<ReadinessBadge … />} back={{ label: "Models", search: { page: "models" } }}
 *     description="…" actions={<Button>Edit</Button>} facts={[{ label: "API name", value: … }]}
 *     sections={[{ id: "routes", title: "Routes", content: … }]} />
 */
import type { ReactNode } from "react";
import { LayoutDashboard } from "lucide-react";
import { ResourcePage, type ResourceTab } from "../resource-page";
import type { DashboardSearch } from "../../lib/permissions";
import { SectionHeadings } from "../ui";
import { Card } from "../ui/card/card";
import { DescriptionList, type DescriptionEntry } from "../ui/description-list/description-list";
import { Stack } from "../ui/layout/layout";
import { PageHeader } from "../ui/page-header/page-header";
import { useResourceName } from "../layout/breadcrumbs";
import { BackLink } from "./form-page";
import s from "../../pages/shared.module.css";
import styles from "./templates.module.css";

/**
 * A titled card; `hidden` leaves it out (e.g. a control the viewer can't use). `id` is its in-page anchor,
 * or its tab value in tabbed mode. In tabbed mode `overview` sections stay on the Overview tab under Details;
 * every other section becomes its own pill tab (`tabLabel`, `icon`, `count`).
 */
export type RecordSection = { id: string; title: string; description?: ReactNode; actions?: ReactNode; content: ReactNode; hidden?: boolean; overview?: boolean; tabLabel?: string; icon?: ReactNode; count?: number };

const sectionCard = (section: RecordSection) => <Card key={section.id} id={section.id} className={styles.recordSection} title={section.title} titleAs="h2" description={section.description} actions={section.actions}><SectionHeadings>{section.content}</SectionHeadings></Card>;

export function RecordPage({ title, meta, description, back, actions, facts, sections, children, tab, onTabChange }: { title: string; meta?: ReactNode; description?: ReactNode; back: { label: string; search: DashboardSearch }; actions?: ReactNode; facts?: DescriptionEntry[]; sections?: RecordSection[]; children?: ReactNode; tab?: string; onTabChange?: (tab: string) => void }) {
  const shown = facts?.filter(f => f.value !== undefined && f.value !== null);
  if (onTabChange) {
    // Pill tabs (Grounded DetailPage style, same as Team/Project pages); the URL keeps the active tab.
    const visible = sections?.filter(section => !section.hidden) ?? [];
    const overview = <Stack gap={6}>{shown && shown.length > 0 && <Card title="Details" titleAs="h2"><DescriptionList items={shown} dividers /></Card>}{visible.filter(section => section.overview).map(sectionCard)}{children}</Stack>;
    const tabs: ResourceTab[] = [{ value: "overview", label: "Overview", icon: <LayoutDashboard aria-hidden />, content: overview }, ...visible.filter(section => !section.overview).map(section => ({ value: section.id, label: section.tabLabel ?? section.title, icon: section.icon, count: section.count, content: sectionCard(section) }))];
    return <ResourcePage title={title} meta={meta} description={description} actions={actions ? <div className={styles.headerActions}>{actions}</div> : undefined} tabs={tabs} tab={tab} onTabChange={onTabChange} />;
  }
  return <RecordStack title={title} meta={meta} description={description} back={back} actions={actions} shown={shown} sections={sections}>{children}</RecordStack>;
}

function RecordStack({ title, meta, description, back, actions, shown, sections, children }: { title: string; meta?: ReactNode; description?: ReactNode; back: { label: string; search: DashboardSearch }; actions?: ReactNode; shown?: DescriptionEntry[]; sections?: RecordSection[]; children?: ReactNode }) {
  useResourceName(title);
  return <Stack gap={6} className={`${s.page} ${styles.takeover}`}>
    <PageHeader title={title} meta={meta} description={description} breadcrumbs={<BackLink {...back} />} actions={actions ? <div className={styles.headerActions}>{actions}</div> : undefined} />
    {shown && shown.length > 0 && <Card title="Details" titleAs="h2"><DescriptionList items={shown} dividers /></Card>}
    {sections?.filter(section => !section.hidden).map(sectionCard)}
    {children}
  </Stack>;
}
