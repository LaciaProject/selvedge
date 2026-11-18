import { Button } from "@cloudflare/kumo/components/button";
import { Input } from "@cloudflare/kumo/components/input";
import { Link } from "@cloudflare/kumo/components/link";
import { Table } from "@cloudflare/kumo/components/table";
import { PlusIcon, QueueIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { FormDialog } from "../../components/dialogs";
import { Async, EmptyState, Page, Section, Time } from "../../components/ui";
import { useAccount, useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import type { Queue } from "../../lib/types";

export function Queues() {
  const { t } = useTranslation();
  const account = useAccount();
  const { call } = useAccountApi();
  const queues = useAccountQuery<Queue[]>(["queues"], "/queues");
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const create = useChange((queue_name: string) => call("/queues", { method: "POST", json: { queue_name } }), {
    done: (_, name) => t("queues.created", { name }),
    invalidate: [["queues"]],
    silentError: true,
  });
  return (
    <Page
      title={t("nav.queues")}
      description={t("queues.description")}
      actions={
        <Button variant="primary" icon={PlusIcon} onClick={() => setCreating(true)}>
          {t("queues.create")}
        </Button>
      }
    >
      <Section title={t("queues.list")} flush>
        <Async
          query={queues}
          empty={(q) => q.length === 0}
          emptyState={<EmptyState icon={<QueueIcon size={32} />} title={t("queues.empty")} description={t("queues.emptyHelp")} />}
        >
          {(list) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("common.name")}</Table.Head>
                  <Table.Head>{t("queues.consumers")}</Table.Head>
                  <Table.Head>{t("common.created")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {list.map((q) => (
                  <Table.Row key={q.queue_id}>
                    <Table.Cell>
                      <Link href={`/accounts/${account}/queues/${q.queue_id}`}>{q.queue_name}</Link>
                    </Table.Cell>
                    <Table.Cell>
                      {q.consumers.length ? q.consumers.map((c) => c.script_name ?? c.type).join(", ") : <span className="text-kumo-subtle">{t("queues.noConsumer")}</span>}
                    </Table.Cell>
                    <Table.Cell>
                      <Time value={q.created_on} />
                    </Table.Cell>
                  </Table.Row>
                ))}
              </Table.Body>
            </Table>
          )}
        </Async>
      </Section>
      <FormDialog
        open={creating}
        onOpenChange={(o) => {
          setCreating(o);
          if (!o) setName("");
        }}
        title={t("queues.create")}
        submitLabel={t("common.create")}
        disabled={!/^[a-z0-9][a-z0-9-_]*$/i.test(name.trim())}
        onSubmit={async () => {
          await create.mutateAsync(name.trim());
        }}
      >
        <Input label={t("common.name")} value={name} onChange={(e) => setName(e.target.value)} autoComplete="off" />
      </FormDialog>
    </Page>
  );
}
