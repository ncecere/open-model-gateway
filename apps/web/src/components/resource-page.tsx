import type { ReactNode } from "react";
import { ActionProvider, Heading, SectionHeadings } from "./ui";
import { Tabs, TabsList, Tab, TabsPanel } from "./ui/tabs/tabs";

export type ResourceTab = { value: string; label: string; content: ReactNode };

/** Gateway composition, not a fork of Bitop: URL state lives at the caller.
 * Only the active panel mounts. Changing tabs also destroys transient secrets.
 */
export function ResourcePage({ title, description, actions, tabs, tab, onTabChange }: {
  title: string; description?: ReactNode; actions?: ReactNode;
  tabs: ResourceTab[]; tab?: string; onTabChange: (tab: string) => void;
}) {
  const active = tabs.find(item => item.value === tab) ?? tabs[0];
  return <section className="resource-page">
    <Heading title={title} description={description} actions={actions} />
    {active && <Tabs value={active.value} onValueChange={value => onTabChange(String(value))}>
      <TabsList variant="pills" className="gateway-pill-list resource-tabs" aria-label={`${title} sections`}>
        {tabs.map(item => <Tab key={item.value} value={item.value}>{item.label}</Tab>)}
      </TabsList>
      <TabsPanel key={active.value} value={active.value}>
        <ActionProvider key={active.value}><SectionHeadings>{active.content}</SectionHeadings></ActionProvider>
      </TabsPanel>
    </Tabs>}
  </section>;
}
