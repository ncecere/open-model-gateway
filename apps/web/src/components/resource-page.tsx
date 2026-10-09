import { useEffect, type ReactNode } from "react";
import { ActionProvider, Heading, SectionHeadings } from "./ui";
import { Tabs, TabsList, Tab, TabsPanel } from "./ui/tabs/tabs";
import { Stack } from "./ui/layout/layout";
import { PageHeader } from "./ui/page-header/page-header";
import { TitleWithIcon } from "./provider-icon";
import { useCrumbTail, useResourceName } from "./layout/breadcrumbs";
import s from "../pages/shared.module.css";
import t from "./resource-page.module.css";
/** `icon` and `count` follow Grounded's DetailPage pill tabs ("Members 3"). */
export type ResourceTab = { value: string; label: string; content: ReactNode; icon?: ReactNode; count?: number };
/** `meta` (badges beside the title) and `facts` (one line under it) switch to Grounded's detail header. */
export function ResourcePage({ title, icon, description, actions, tabs, tab, onTabChange, meta, facts, notices }: { title: string; /** A logo left of the title (TitleWithIcon). */ icon?: ReactNode; description?: ReactNode; actions?: ReactNode; tabs: ResourceTab[]; tab?: string; onTabChange: (tab: string) => void; meta?: ReactNode; facts?: ReactNode; notices?: ReactNode }) {
  const active = tabs.find(item => item.value === tab) ?? tabs[0];
  useResourceName(title);
  useCrumbTail(active && active !== tabs[0] ? active.label : undefined);
  useEffect(() => { if (tab && !tabs.some(item => item.value === tab) && active) onTabChange(active.value); }, [tab, active?.value]);
  return <Stack gap={6} className={s.page}>{meta || facts || icon ? <PageHeader title={icon ? <TitleWithIcon icon={icon}>{title}</TitleWithIcon> : title} meta={meta} description={description} facts={facts} actions={actions} /> : <Heading title={title} description={description} actions={actions} />}{notices}{active && <Tabs value={active.value} onValueChange={value => onTabChange(String(value))}><TabsList variant="pills" className={t.list} aria-label={`${title} sections`}>{tabs.map(item => <Tab key={item.value} value={item.value} icon={item.icon} count={item.count}>{item.label}</Tab>)}</TabsList><TabsPanel key={active.value} value={active.value}><ActionProvider key={active.value}><SectionHeadings>{active.content}</SectionHeadings></ActionProvider></TabsPanel></Tabs>}</Stack>;
}
