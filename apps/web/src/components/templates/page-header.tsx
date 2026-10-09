/*
 * The gateway's page header: Bitop's PageHeader with its actions in
 * HeaderActions, so every page/record header collapses to primary + "⋯" on a
 * phone. Use this instead of importing the vendored PageHeader in pages.
 * (Form pages keep the vendored header: Cancel + submit are both needed.)
 */
import { PageHeader as BitopPageHeader, type PageHeaderProps } from "../ui/page-header/page-header";
import { HeaderActions } from "./header-actions";

export type { PageHeaderProps };

export function PageHeader({ actions, ...props }: PageHeaderProps) {
  return <BitopPageHeader {...props} actions={actions ? <HeaderActions>{actions}</HeaderActions> : undefined} />;
}
