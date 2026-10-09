/*
 * Top-bar bell: unread alert count, linking to the routed Notifications page
 * (no drawer). Polls the cheap summary endpoint once a minute and on focus.
 */
import { useQuery } from "@tanstack/react-query";
import { Bell } from "lucide-react";
import { api } from "../lib/api";
import { notificationSummaryPath, type NotificationSummary } from "../lib/alerts";
import { useApiScope } from "./ui";
import { Button } from "./ui/button/button";
import { ResourceLink } from "./navigation-link";
import s from "./notification-bell.module.css";

export const bellLabel = (unread: number) => unread > 0 ? `Notifications, ${unread} unread` : "Notifications";
export const bellCount = (unread: number) => unread > 99 ? "99+" : String(unread);

export function NotificationBell() {
  const scope = useApiScope();
  const q = useQuery({ queryKey: ["api", scope, notificationSummaryPath], queryFn: ({ signal }) => api<NotificationSummary>(notificationSummaryPath, { signal }), retry: false, refetchInterval: 60_000, refetchOnWindowFocus: true });
  const unread = q.data?.unread ?? 0, label = bellLabel(unread);
  return <Button variant="ghost" size="sm" iconOnly aria-label={label} title={label} className={s.bell} render={<ResourceLink search={{ page: "notifications" }} />}>
    <Bell aria-hidden />{unread > 0 && <span className={s.count} aria-hidden>{bellCount(unread)}</span>}
  </Button>;
}
