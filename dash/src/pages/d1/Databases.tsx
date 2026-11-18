import { Button } from "@cloudflare/kumo/components/button";
import { Input } from "@cloudflare/kumo/components/input";
import { Link } from "@cloudflare/kumo/components/link";
import { Table } from "@cloudflare/kumo/components/table";
import { DatabaseIcon, PlusIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { FormDialog } from "../../components/dialogs";
import { Async, EmptyState, Mono, Page, Section, Time } from "../../components/ui";
import { useFormat } from "../../lib/format";
import { useAccount, useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import type { D1Database } from "../../lib/types";

export function Databases() {
  const { t } = useTranslation();
  const f = useFormat();
  const account = useAccount();
  const { call } = useAccountApi();
  const dbs = useAccountQuery<D1Database[]>(["d1"], "/d1/database");
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const create = useChange((name: string) => call("/d1/database", { method: "POST", json: { name } }), {
    done: (_, name) => t("d1.created", { name }),
    invalidate: [["d1"]],
    silentError: true,
  });
  return (
    <Page
      title={t("nav.d1")}
      description={t("d1.description")}
      actions={
        <Button variant="primary" icon={PlusIcon} onClick={() => setCreating(true)}>
          {t("d1.create")}
        </Button>
      }
    >
      <Section title={t("d1.databases")} flush>
        <Async
          query={dbs}
          empty={(d) => d.length === 0}
          emptyState={<EmptyState icon={<DatabaseIcon size={32} />} title={t("d1.empty")} description={t("d1.emptyHelp")} />}
        >
          {(list) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("common.name")}</Table.Head>
                  <Table.Head>{t("common.id")}</Table.Head>
                  <Table.Head>{t("d1.size")}</Table.Head>
                  <Table.Head>{t("common.created")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {list.map((d) => (
                  <Table.Row key={d.uuid}>
                    <Table.Cell>
                      <Link href={`/accounts/${account}/d1/${d.uuid}`}>{d.name}</Link>
                    </Table.Cell>
                    <Table.Cell>
                      <Mono className="text-kumo-subtle">{d.uuid}</Mono>
                    </Table.Cell>
                    <Table.Cell>{f.bytes(d.file_size)}</Table.Cell>
                    <Table.Cell>
                      <Time value={d.created_at} />
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
        title={t("d1.create")}
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
