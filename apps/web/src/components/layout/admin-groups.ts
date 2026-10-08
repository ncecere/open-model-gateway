import { useCallback, useState } from "react";
const known = ["People", "Models", "Usage & spend", "Records"];
const key = "omg.enterprise.adminNavOpen";
export function currentGroups(saved: string[]) { return [...new Set(saved)].filter(label => known.includes(label)); }
function read() { try { const parsed: unknown = JSON.parse(globalThis.localStorage?.getItem(key) ?? "[]"); return currentGroups(Array.isArray(parsed) ? parsed.filter((x): x is string => typeof x === "string") : []); } catch { return []; } }
export function useAdminGroups(active?: string) {
  const [saved, setSaved] = useState<string[]>(read);
  const [closed, setClosed] = useState({ active, closed: false });
  if (closed.active !== active) setClosed({ active, closed: false });
  const setOpen = useCallback((label: string, open: boolean) => { if (label === active) setClosed({ active, closed: !open }); setSaved(list => { const next = open ? [...list.filter(l => l !== label), label] : list.filter(l => l !== label); try { localStorage.setItem(key, JSON.stringify(next)); } catch { /* Unavailable storage. */ } return next; }); }, [active]);
  return { isOpen: (label: string) => label === active ? !(closed.active === active && closed.closed) : saved.includes(label), setOpen };
}
