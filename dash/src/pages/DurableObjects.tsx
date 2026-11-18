import { Badge } from "@cloudflare/kumo/components/badge";
import { Link } from "@cloudflare/kumo/components/link";
import { Table } from "@cloudflare/kumo/components/table";
import { ShapesIcon } from "@phosphor-icons/react";
import { useTranslation } from "react-i18next";

import { Async, EmptyState, Mono, Page, Section } from "../components/ui";
import { useAccount, useAccountQuery } from "../lib/hooks";
import type { DoNamespace } from "../lib/types";

export function DurableObjects() {
  const { t } = useTranslation();
  const account = useAccount();
  const namespaces = useAccountQuery<DoNamespace[]>(["do"], "/workers/durable_objects/namespaces");
  return (
    <Page title={t("nav.durableObjects")} description={t("durableObjects.description")}>
      <Section title={t("durableObjects.namespaces")} flush>
        <Async
          query={namespaces}
          empty={(n) => n.length === 0}
          emptyState={<EmptyState icon={<ShapesIcon size={32} />} title={t("durableObjects.empty")} description={t("durableObjects.emptyHelp")} />}
        >
          {(list) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("durableObjects.class")}</Table.Head>
                  <Table.Head>{t("workflows.worker")}</Table.Head>
                  <Table.Head>{t("durableObjects.storage")}</Table.Head>
                  <Table.Head>{t("common.id")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {list.map((n) => (
                  <Table.Row key={n.id}>
                    <Table.Cell>{n.class}</Table.Cell>
                    <Table.Cell>
                      <Link href={`/accounts/${account}/workers/${encodeURIComponent(n.script)}`}>{n.script}</Link>
                    </Table.Cell>
                    <Table.Cell>
                      <Badge variant="neutral">{n.use_sqlite ? "SQLite" : t("durableObjects.kvStorage")}</Badge>
                    </Table.Cell>
                    <Table.Cell>
                      <Mono className="text-kumo-subtle">{n.id}</Mono>
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
