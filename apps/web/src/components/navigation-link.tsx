import { createContext, forwardRef, useContext, type AnchorHTMLAttributes, type MouseEventHandler, type ReactNode } from "react";
import type { DashboardSearch } from "../lib/permissions";
import { dashboardHref } from "../lib/locations";
import { TextLink } from "./ui/text-link/text-link";
type Navigation = { search: DashboardSearch; navigate: (search: DashboardSearch) => void };
const DashboardNavigation = createContext<Navigation | undefined>(undefined);
export function DashboardNavigationProvider({ search, navigate, children }: Navigation & { children: ReactNode }) { return <DashboardNavigation.Provider value={{ search, navigate }}>{children}</DashboardNavigation.Provider>; }
export function useDashboardNavigation() { return useContext(DashboardNavigation); }
/** Real anchors preserve copied URLs, native modified clicks, new tabs and no-JS fallback.
 * Inline content uses Bitop TextLink; shell render slots supply their own root styles. */
export const ResourceLink = forwardRef<HTMLAnchorElement, Omit<AnchorHTMLAttributes<HTMLAnchorElement>, "href"> & { search: DashboardSearch }>(function ResourceLink({ search, children, className, onClick, ...props }, ref) {
  const navigation = useDashboardNavigation();
  const activate: MouseEventHandler<HTMLAnchorElement> = event => { onClick?.(event); if (!navigation || event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || props.target && props.target !== "_self" || props.download !== undefined) return; event.preventDefault(); navigation.navigate(search); };
  return className ? <a {...props} ref={ref} className={className} href={dashboardHref(search)} onClick={activate}>{children}</a> : <TextLink {...props} ref={ref} href={dashboardHref(search)} onClick={activate}>{children}</TextLink>;
});
