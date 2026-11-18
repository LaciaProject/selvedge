import { Button } from "@cloudflare/kumo/components/button";
import { Input } from "@cloudflare/kumo/components/input";
import { Link } from "@cloudflare/kumo/components/link";
import { Table } from "@cloudflare/kumo/components/table";
import { PackageIcon, PlusIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { FormDialog } from "../../components/dialogs";
import { Async, EmptyState, Page, Section, Time } from "../../components/ui";
import { useAccount, useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import type { Bucket } from "../../lib/types";

/** R2's rule: 3-63 lowercase letters, digits and hyphens, not at either end. */
export const validBucketName = (n: string) => /^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$/.test(n);

export function Buckets() {
  const { t } = useTranslation();
  const account = useAccount();
  const { call } = useAccountApi();
  const buckets = useAccountQuery<{ buckets: Bucket[] }>(["r2"], "/r2/buckets");
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const create = useChange((name: string) => call("/r2/buckets", { method: "POST", json: { name } }), {
    done: (_, name) => t("r2.created", { name }),
    invalidate: [["r2"]],
    silentError: true,
  });
  return (
    <Page
      title={t("nav.r2")}
      description={t("r2.description")}
      actions={
        <Button variant="primary" icon={PlusIcon} onClick={() => setCreating(true)}>
          {t("r2.create")}
        </Button>
      }
    >
      <Section title={t("r2.buckets")} flush>
        <Async
          query={buckets}
          empty={(b) => b.buckets.length === 0}
          emptyState={<EmptyState icon={<PackageIcon size={32} />} title={t("r2.empty")} description={t("r2.emptyHelp")} />}
        >
          {(b) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("common.name")}</Table.Head>
                  <Table.Head>{t("r2.storageClass")}</Table.Head>
                  <Table.Head>{t("common.created")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {b.buckets.map((bucket) => (
                  <Table.Row key={bucket.name}>
                    <Table.Cell>
                      <Link href={`/accounts/${account}/r2/${encodeURIComponent(bucket.name)}`}>{bucket.name}</Link>
                    </Table.Cell>
                    <Table.Cell>{bucket.storage_class ?? "Standard"}</Table.Cell>
                    <Table.Cell>
                      <Time value={bucket.creation_date} />
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
        title={t("r2.create")}
        submitLabel={t("common.create")}
        disabled={!validBucketName(name.trim())}
        onSubmit={async () => {
          await create.mutateAsync(name.trim());
        }}
      >
        <Input
          label={t("common.name")}
          description={t("r2.nameRule")}
          value={name}
          onChange={(e) => setName(e.target.value)}
          autoComplete="off"
        />
      </FormDialog>
    </Page>
  );
}
