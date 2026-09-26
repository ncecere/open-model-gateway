import { useRef, useState } from "react";
import type { Organization, Session, Workspace } from "../lib/api";
import type { DashboardSearch } from "../lib/permissions";
import { jumpTargets } from "../lib/search";
import { CommandPalette, CommandPaletteTrigger, useCommandPaletteShortcut, type CommandGroup } from "./ui/command-palette/command-palette";

export function JumpSearch({ session, organization, workspace, navigate, refresh }: {
  session: Session; organization?: Organization; workspace?: Workspace;
  navigate: (search: DashboardSearch) => void; refresh: () => void;
}) {
  const [open, setOpen] = useState(false);
  const navigated = useRef(false);
  const changeOpen = (next: boolean) => {
    if (next) navigated.current = false;
    setOpen(next);
  };
  useCommandPaletteShortcut(() => {
    // Do not stack this palette over a confirmation or one-time-secret dialog.
    if (!open && document.querySelector("dialog[open]")) return;
    changeOpen(!open);
  });
  const groups: CommandGroup[] = [];
  for (const target of jumpTargets(session, organization, workspace)) {
    let group = groups.find(group => group.label === target.group);
    if (!group) { group = { label: target.group, items: [] }; groups.push(group); }
    group.items.push({ id: target.id, label: target.label, hint: target.hint, keywords: target.keywords, onSelect: () => { navigated.current = true; navigate(target.search); } });
  }
  groups.push({ label: "Actions", items: [{ id: "refresh", label: "Refresh access", keywords: ["reload", "permissions"], onSelect: refresh }] });
  return <>
    <CommandPaletteTrigger label="Search or jump to…" className="gateway-search" onClick={() => changeOpen(true)} />
    <CommandPalette className="gateway-palette" open={open} onOpenChange={changeOpen} finalFocus={() => {
      if (!navigated.current) return true;
      const heading = document.querySelector<HTMLElement>("#main h1");
      if (!heading) return true;
      heading.tabIndex = -1;
      return heading;
    }} groups={groups} label="Search or jump to" placeholder="Search pages, organizations, teams and projects…" emptyText="No accessible pages or workspaces match your search." />
  </>;
}
