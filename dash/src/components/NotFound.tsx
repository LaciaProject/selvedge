import { Button } from "@cloudflare/kumo/components/button";
import { useTranslation } from "react-i18next";

import { EmptyState } from "./ui";

export function NotFound() {
  const { t } = useTranslation();
  return (
    <div className="p-12">
      <EmptyState
        title={t("notFound.title")}
        description={t("notFound.description")}
        action={
          <Button variant="secondary" onClick={() => window.location.assign("/")}>
            {t("notFound.home")}
          </Button>
        }
      />
    </div>
  );
}
