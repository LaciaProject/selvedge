import "./styles.css";

import { Toasty } from "@cloudflare/kumo/components/toast";
import { TooltipProvider } from "@cloudflare/kumo/components/tooltip";
import { KumoLocaleProvider, type LinkComponentProps, LinkProvider } from "@cloudflare/kumo/utils";
import { QueryClientProvider } from "@tanstack/react-query";
import { Link, RouterProvider } from "@tanstack/react-router";
import { forwardRef, StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { useTranslation } from "react-i18next";

import { initI18n } from "./i18n";
import { setUnauthorizedHandler } from "./lib/api";
import { queryClient } from "./lib/session";
import { toasts } from "./lib/toast";
import { router } from "./router";

// Kumo's links go through the router; links elsewhere stay plain.
const AppLink = forwardRef<HTMLAnchorElement, LinkComponentProps>(({ href, to, ...rest }, ref) => {
  const target = href ?? to ?? "";
  if (/^[a-z]+:/i.test(target)) return <a ref={ref} href={target} {...rest} />;
  return <Link ref={ref} to={target} {...rest} />;
});
AppLink.displayName = "AppLink";

function Providers() {
  const { t } = useTranslation();
  return (
    <KumoLocaleProvider
      translations={{
        layerDialog: { close: t("common.close"), cancel: t("common.cancel") },
        breadcrumbs: { label: t("common.breadcrumbs"), copy: t("common.copy"), copyTooltip: t("common.copy") },
        label: { optional: t("common.optional"), tooltip: t("common.moreInfo") },
      }}
    >
      <LinkProvider component={AppLink}>
        <TooltipProvider>
          <Toasty toastManager={toasts}>
            <QueryClientProvider client={queryClient}>
              <RouterProvider router={router} />
            </QueryClientProvider>
          </Toasty>
        </TooltipProvider>
      </LinkProvider>
    </KumoLocaleProvider>
  );
}

setUnauthorizedHandler(() => {
  const here = window.location.pathname + window.location.search;
  if (!window.location.pathname.startsWith("/login")) {
    queryClient.clear();
    router.navigate({ to: "/login", search: { return_to: here, error: undefined } });
  }
});

initI18n().then(() => {
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <Providers />
    </StrictMode>,
  );
});
