import { Navigate, useRouteContext } from "@tanstack/react-router";
import { useTranslation } from "react-i18next";

import { EmptyState, Page } from "../components/ui";

/** The first account, or the admin pages, or a word about having none. */
export function Home() {
  const { t } = useTranslation();
  const { me } = useRouteContext({ from: "/app" });
  if (me.accounts.length > 0) {
    return <Navigate to="/accounts/$account" params={{ account: me.accounts[0].id }} replace />;
  }
  if (me.admin) return <Navigate to="/admin" search={{ tab: undefined }} replace />;
  return (
    <Page title={t("home.title")}>
      <EmptyState title={t("home.noAccounts")} description={t("home.noAccountsHelp")} />
    </Page>
  );
}
