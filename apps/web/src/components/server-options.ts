/*
 * Options searched on the server (R06, #69): the first 20 matches for the typed
 * text, never a whole list of keys, workspaces or people. A selected value that
 * isn't among them is labelled by fetching that one record.
 */
import { useEffect, useState } from "react";
import type { Collection } from "../lib/api";
import type { FacetOption } from "./ui/filter-bar/filter-bar";
import { useApi } from "./ui";

/** `path` takes `q` and `limit`; an absent path searches nothing. */
export function useServerOptions<T>(path: string | undefined, toOption: (row: T) => FacetOption | undefined) {
  const [text, setText] = useState(""), [q, setQ] = useState("");
  useEffect(() => { const timer = setTimeout(() => setQ(text.trim()), 250); return () => clearTimeout(timer); }, [text]);
  const on = !!path && q.length <= 200, sep = path?.includes("?") ? "&" : "?";
  const found = useApi<Collection<T>>(on ? `${path}${sep}${q ? `q=${encodeURIComponent(q)}&` : ""}limit=20` : "", on);
  const options = (found.data?.data ?? []).map(toOption).filter((o): o is FacetOption => !!o);
  // Search state, so a page can tell a failed search from an empty one.
  const emptyText = found.isFetching ? "Searching…" : found.isError ? "Search failed. Try again." : undefined;
  return { options, onSearch: setText, isError: found.isError, error: found.error, refetch: found.refetch, isFetching: found.isFetching, emptyText };
}

/** The name of one selected record (`path` is its detail URL), or `fallback` until it loads or when it can't. */
export function useSelectedName<T>(path: string | undefined, name: (row: T) => string | undefined, fallback: string): string {
  const one = useApi<T>(path ?? "", !!path);
  return (one.data && name(one.data)) || fallback;
}

/** Puts the selected value first when the current matches don't include it. */
export function withSelected(options: FacetOption[], selected: FacetOption | undefined): FacetOption[] {
  return selected && !options.some(o => o.value === selected.value) ? [selected, ...options] : options;
}
