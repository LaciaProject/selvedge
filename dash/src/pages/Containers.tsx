import { Badge } from "@cloudflare/kumo/components/badge";
import { Table } from "@cloudflare/kumo/components/table";
import { CubeIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { FormDialog } from "../components/dialogs";
import { Async, EmptyState, Mono, Page, Section, Time } from "../components/ui";
import { ApiError } from "../lib/api";
import { useAccountQuery } from "../lib/hooks";
import type { ContainerApp } from "../lib/types";

type Rollout = { id: string; created_at: string; status: string; kind: string; strategy: string; description: string };

export function Containers() {
  const { t } = useTranslation();
  const apps = useAccountQuery<ContainerApp[]>(["containers"], "/containers/applications");
  const [open, setOpen] = useState<ContainerApp>();
  // Without a registry the containers API is off.
  const off = apps.error instanceof ApiError && (apps.error.status === 404 || apps.error.status === 501);
  return (
    <Page title={t("nav.containers")} description={t("containers.description")}>
      <Section title={t("containers.applications")} flush>
        {off ? (
          <EmptyState icon={<CubeIcon size={32} />} title={t("containers.disabled")} description={t("containers.disabledHelp")} />
        ) : (
          <Async
            query={apps}
            empty={(a) => a.length === 0}
            emptyState={<EmptyState icon={<CubeIcon size={32} />} title={t("containers.empty")} description={t("containers.emptyHelp")} />}
          >
            {(list) => (
              <Table>
                <Table.Header>
                  <Table.Row>
                    <Table.Head>{t("common.name")}</Table.Head>
                    <Table.Head>{t("containers.image")}</Table.Head>
                    <Table.Head>{t("containers.instanceType")}</Table.Head>
                    <Table.Head>{t("containers.maxInstances")}</Table.Head>
                    <Table.Head>{t("common.created")}</Table.Head>
                  </Table.Row>
                </Table.Header>
                <Table.Body>
                  {list.map((a) => (
                    <Table.Row key={a.id}>
                      <Table.Cell>
                        <button type="button" className="text-kumo-link hover:underline" onClick={() => setOpen(a)}>
                          {a.name}
                        </button>
                      </Table.Cell>
                      <Table.Cell>
                        <Mono>{a.configuration?.image ?? "—"}</Mono>
                      </Table.Cell>
                      <Table.Cell>
                        <Badge variant="neutral">{a.configuration?.instance_type ?? "—"}</Badge>
                      </Table.Cell>
                      <Table.Cell>{a.max_instances ?? "—"}</Table.Cell>
                      <Table.Cell>
                        <Time value={a.created_at} />
                      </Table.Cell>
                    </Table.Row>
                  ))}
                </Table.Body>
              </Table>
            )}
          </Async>
        )}
      </Section>
      {open && <AppDetails app={open} onClose={() => setOpen(undefined)} />}
    </Page>
  );
}

function AppDetails({ app, onClose }: { app: ContainerApp; onClose: () => void }) {
  const { t } = useTranslation();
  const rollouts = useAccountQuery<Rollout[]>(["container-rollouts", app.id], `/containers/applications/${app.id}/rollouts`);
  return (
    <FormDialog open onOpenChange={(o) => !o && onClose()} title={app.name} size="xl" submitLabel={t("common.close")} onSubmit={async () => {}}>
      <div className="flex flex-col gap-2">
        <span className="font-medium">{t("containers.configuration")}</span>
        <pre className="max-h-64 overflow-auto rounded-md bg-kumo-tint p-3 font-mono text-xs">{JSON.stringify(app.configuration, null, 2)}</pre>
      </div>
      <div className="flex flex-col gap-2">
        <span className="font-medium">{t("containers.rollouts")}</span>
        <span className="text-sm text-kumo-subtle">{t("containers.rolloutsHelp")}</span>
        <Async query={rollouts} empty={(r) => r.length === 0} emptyState={<EmptyState title={t("containers.noRollouts")} />}>
          {(list) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("common.created")}</Table.Head>
                  <Table.Head>{t("workflows.status")}</Table.Head>
                  <Table.Head>{t("containers.kind")}</Table.Head>
                  <Table.Head>{t("containers.strategy")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {list.map((r) => (
                  <Table.Row key={r.id}>
                    <Table.Cell>
                      <Time value={r.created_at} />
                    </Table.Cell>
                    <Table.Cell>
                      <Badge variant="success">{r.status}</Badge>
                    </Table.Cell>
                    <Table.Cell>{r.kind}</Table.Cell>
                    <Table.Cell>{r.strategy}</Table.Cell>
                  </Table.Row>
                ))}
              </Table.Body>
            </Table>
          )}
        </Async>
      </div>
    </FormDialog>
  );
}
