import { Button } from "@cloudflare/kumo/components/button";
import { Input } from "@cloudflare/kumo/components/input";
import { Link } from "@cloudflare/kumo/components/link";
import { Table } from "@cloudflare/kumo/components/table";
import { KeyIcon, PlusIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { FormDialog } from "../../components/dialogs";
import { Async, EmptyState, Mono, Page, Section } from "../../components/ui";
import { useAccount, useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import type { KvNamespace } from "../../lib/types";

export function Namespaces() {
  const { t } = useTranslation();
  const account = useAccount();
  const { call } = useAccountApi();
  const namespaces = useAccountQuery<KvNamespace[]>(["kv"], "/storage/kv/namespaces?per_page=100");
  const [creating, setCreating] = useState(false);
  const [title, setTitle] = useState("");
  const create = useChange((title: string) => call("/storage/kv/namespaces", { method: "POST", json: { title } }), {
    done: (_, title) => t("kv.created", { name: title }),
    invalidate: [["kv"]],
    silentError: true,
  });
  return (
    <Page
      title={t("nav.kv")}
      description={t("kv.description")}
      actions={
        <Button variant="primary" icon={PlusIcon} onClick={() => setCreating(true)}>
          {t("kv.create")}
        </Button>
      }
    >
      <Section title={t("kv.namespaces")} flush>
        <Async
          query={namespaces}
          empty={(n) => n.length === 0}
          emptyState={<EmptyState icon={<KeyIcon size={32} />} title={t("kv.empty")} description={t("kv.emptyHelp")} />}
        >
          {(list) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("common.name")}</Table.Head>
                  <Table.Head>{t("common.id")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {list.map((ns) => (
                  <Table.Row key={ns.id}>
                    <Table.Cell>
                      <Link href={`/accounts/${account}/kv/${ns.id}`}>{ns.title}</Link>
                    </Table.Cell>
                    <Table.Cell>
                      <Mono className="text-kumo-subtle">{ns.id}</Mono>
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
          if (!o) setTitle("");
        }}
        title={t("kv.create")}
        submitLabel={t("common.create")}
        disabled={!title.trim()}
        onSubmit={async () => {
          await create.mutateAsync(title.trim());
        }}
      >
        <Input label={t("common.name")} value={title} onChange={(e) => setTitle(e.target.value)} autoComplete="off" />
      </FormDialog>
    </Page>
  );
}
