import { Button } from "@cloudflare/kumo/components/button";
import { TrashIcon } from "@phosphor-icons/react";
import { useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Crumbs } from "../../components/Crumbs";
import { ConfirmDelete } from "../../components/dialogs";
import { UrlTabs } from "../../components/TabbedPage";
import { Page } from "../../components/ui";
import { useAccountApi, useChange } from "../../lib/hooks";
import { Deployments } from "./Deployments";
import { Logs } from "./Logs";
import { Settings } from "./Settings";
import { Triggers } from "./Triggers";

export function Worker() {
  const { t } = useTranslation();
  const { script } = useParams({ strict: false }) as { script: string };
  const search = useSearch({ strict: false }) as { tab?: string };
  const { account, call } = useAccountApi();
  const navigate = useNavigate();
  const [deleting, setDeleting] = useState(false);
  const remove = useChange(() => call(`/workers/scripts/${encodeURIComponent(script)}`, { method: "DELETE" }), {
    done: t("workers.deleted", { name: script }),
    invalidate: [["scripts"]],
    silentError: true,
  });

  return (
    <Page
      crumbs={<Crumbs items={[{ label: t("nav.workers"), href: `/accounts/${account}/workers` }]} current={script} />}
      title={script}
      actions={
        <Button variant="secondary-destructive" icon={TrashIcon} onClick={() => setDeleting(true)}>
          {t("common.delete")}
        </Button>
      }
    >
      <UrlTabs
        current={search.tab}
        tabs={[
          { value: "deployments", label: t("workers.tabs.deployments"), render: () => <Deployments script={script} /> },
          { value: "settings", label: t("workers.tabs.settings"), render: () => <Settings script={script} /> },
          { value: "triggers", label: t("workers.tabs.triggers"), render: () => <Triggers script={script} /> },
          { value: "logs", label: t("workers.tabs.logs"), render: () => <Logs script={script} /> },
        ]}
      />
      <ConfirmDelete
        open={deleting}
        onOpenChange={setDeleting}
        kind={t("kinds.worker")}
        name={script}
        onConfirm={async () => {
          await remove.mutateAsync();
          navigate({ to: "/accounts/$account/workers", params: { account } });
        }}
      />
    </Page>
  );
}
