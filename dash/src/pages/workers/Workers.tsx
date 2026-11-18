import { Badge } from "@cloudflare/kumo/components/badge";
import { Link } from "@cloudflare/kumo/components/link";
import { Table } from "@cloudflare/kumo/components/table";
import { CodeIcon } from "@phosphor-icons/react";
import { useTranslation } from "react-i18next";

import { Async, EmptyState, Page, Section, Time } from "../../components/ui";
import { useAccount, useAccountQuery } from "../../lib/hooks";
import type { Script } from "../../lib/types";

export function Workers() {
  const { t } = useTranslation();
  const account = useAccount();
  const scripts = useAccountQuery<Script[]>(["scripts"], "/workers/scripts");
  return (
    <Page title={t("nav.workers")} description={t("workers.description")}>
      <Section title={t("workers.list")} flush>
        <Async
          query={scripts}
          empty={(s) => s.length === 0}
          emptyState={
            <EmptyState icon={<CodeIcon size={32} />} title={t("workers.empty")} description={t("workers.emptyHelp")} />
          }
        >
          {(list) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("common.name")}</Table.Head>
                  <Table.Head>{t("workers.handlers")}</Table.Head>
                  <Table.Head>{t("workers.lastDeployed")}</Table.Head>
                  <Table.Head>{t("common.created")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {[...list]
                  .sort((a, b) => a.id.localeCompare(b.id))
                  .map((s) => (
                    <Table.Row key={s.id}>
                      <Table.Cell>
                        <Link href={`/accounts/${account}/workers/${encodeURIComponent(s.id)}`}>{s.id}</Link>
                      </Table.Cell>
                      <Table.Cell>
                        <div className="flex flex-wrap gap-1">
                          {s.handlers.map((h) => (
                            <Badge key={h} variant="neutral">
                              {h}
                            </Badge>
                          ))}
                          {s.has_assets && <Badge variant="info">{t("workers.assets")}</Badge>}
                        </div>
                      </Table.Cell>
                      <Table.Cell>
                        <Time value={s.modified_on} />
                      </Table.Cell>
                      <Table.Cell>
                        <Time value={s.created_on} />
                      </Table.Cell>
                    </Table.Row>
                  ))}
              </Table.Body>
            </Table>
          )}
        </Async>
      </Section>
    </Page>
  );
}
