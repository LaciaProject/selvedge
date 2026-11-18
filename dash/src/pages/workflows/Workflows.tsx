import { Link } from "@cloudflare/kumo/components/link";
import { Table } from "@cloudflare/kumo/components/table";
import { FlowArrowIcon } from "@phosphor-icons/react";
import { useTranslation } from "react-i18next";

import { Async, EmptyState, Page, Section, Time } from "../../components/ui";
import { useAccount, useAccountQuery } from "../../lib/hooks";
import type { Workflow } from "../../lib/types";

export function Workflows() {
  const { t } = useTranslation();
  const account = useAccount();
  const workflows = useAccountQuery<Workflow[]>(["workflows"], "/workflows");
  return (
    <Page title={t("nav.workflows")} description={t("workflows.description")}>
      <Section title={t("workflows.list")} flush>
        <Async
          query={workflows}
          empty={(w) => w.length === 0}
          emptyState={<EmptyState icon={<FlowArrowIcon size={32} />} title={t("workflows.empty")} description={t("workflows.emptyHelp")} />}
        >
          {(list) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("common.name")}</Table.Head>
                  <Table.Head>{t("workflows.worker")}</Table.Head>
                  <Table.Head>{t("workflows.class")}</Table.Head>
                  <Table.Head>{t("common.created")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {list.map((w) => (
                  <Table.Row key={w.name}>
                    <Table.Cell>
                      <Link href={`/accounts/${account}/workflows/${encodeURIComponent(w.name)}`}>{w.name}</Link>
                    </Table.Cell>
                    <Table.Cell>
                      <Link href={`/accounts/${account}/workers/${encodeURIComponent(w.script_name)}`}>{w.script_name}</Link>
                    </Table.Cell>
                    <Table.Cell>{w.class_name}</Table.Cell>
                    <Table.Cell>
                      <Time value={w.created_on} />
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
