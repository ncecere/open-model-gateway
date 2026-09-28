import { createContext, forwardRef, useContext, type AnchorHTMLAttributes, type ReactNode } from "react";
import type { DashboardSearch } from "../lib/permissions";
import { dashboardHref } from "../lib/locations";

type Navigation = { search: DashboardSearch; navigate: (search: DashboardSearch) => void };
const DashboardNavigation = createContext<Navigation | undefined>(undefined);
export function DashboardNavigationProvider({ search, navigate, children }: Navigation & { children: ReactNode }) {
  return <DashboardNavigation.Provider value={{ search, navigate }}>{children}</DashboardNavigation.Provider>;
}
export function useDashboardNavigation() { return useContext(DashboardNavigation); }

/** Real links work in isolated renders, new tabs, copied URLs and without JS. */
export const ResourceLink = forwardRef<HTMLAnchorElement, Omit<AnchorHTMLAttributes<HTMLAnchorElement>, "href"> & { search: DashboardSearch }>(function ResourceLink({ search, children, className = "resource-link", onClick, ...props }, ref) {
  const navigation = useDashboardNavigation();
  return <a {...props} ref={ref} className={className} href={dashboardHref(search)} onClick={event => {
    onClick?.(event);
    if (!navigation || event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || (props.target && props.target !== "_self") || props.download !== undefined) return;
    event.preventDefault();
    navigation.navigate(search);
  }}>{children}</a>;
});
