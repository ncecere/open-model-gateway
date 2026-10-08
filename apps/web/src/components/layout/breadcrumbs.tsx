import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import type { BreadcrumbItem } from "../ui/breadcrumbs/breadcrumbs";
import type { DashboardSearch } from "../../lib/permissions";
/** A record's own parent record, shown between the list and the record: Admin › Models › GPT-6 Luna › OpenAI route. */
export type CrumbParent = { label: string; to: DashboardSearch };
const SetParent = createContext<(parent?: CrumbParent) => void>(() => {});
const Parent = createContext<CrumbParent | undefined>(undefined);
const Tail = createContext<(label?: string) => void>(() => {});
const Current = createContext<string | undefined>(undefined);
const SetName = createContext<(name?: string) => void>(() => {});
const Name = createContext<string | undefined>(undefined);
export function CrumbProvider({ children }: { children: ReactNode }) { const [tail, setTail] = useState<string>(); const [name, setName] = useState<string>(); const [parent, setParent] = useState<CrumbParent>(); return <SetParent.Provider value={setParent}><Parent.Provider value={parent}><SetName.Provider value={setName}><Name.Provider value={name}><Tail.Provider value={setTail}><Current.Provider value={tail}>{children}</Current.Provider></Tail.Provider></Name.Provider></SetName.Provider></Parent.Provider></SetParent.Provider>; }
export function useResourceParent(parent?: CrumbParent) { const set = useContext(SetParent), key = parent ? `${parent.label}|${JSON.stringify(parent.to)}` : ""; useEffect(() => { set(parent); return () => set(undefined); }, [key, set]); }
export const useCurrentResourceParent = () => useContext(Parent);
export function useResourceName(name?: string) { const set = useContext(SetName); useEffect(() => { set(name); return () => set(undefined); }, [name, set]); }
export const useCurrentResourceName = () => useContext(Name);
export function useCrumbTail(label?: string) { const set = useContext(Tail); useEffect(() => { set(label); return () => set(undefined); }, [label, set]); }
export const useCurrentCrumbTail = () => useContext(Current);
export function fitCrumbs(trail: BreadcrumbItem[], narrow: boolean): BreadcrumbItem[] {
  const crumbs = trail.filter(c => c.label !== ""); if (!narrow || crumbs.length <= 2) return crumbs;
  const middle = crumbs.slice(1, -1); return [crumbs[0]!, { label: "More pages", collapsed: middle }, crumbs[crumbs.length - 1]!];
}
export function documentTitle(crumbs: BreadcrumbItem[], name: string) { return [...crumbs.map(c => typeof c.label === "string" ? c.label : "").filter(Boolean).slice(-2).reverse(), name].join(" · "); }
