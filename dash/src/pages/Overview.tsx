import { ClipboardText } from "@cloudflare/kumo/components/clipboard-text";
import { Grid } from "@cloudflare/kumo/components/grid";
import { LayerCard } from "@cloudflare/kumo/components/layer-card";
import { Link } from "@cloudflare/kumo/components/link";
import { Table } from "@cloudflare/kumo/components/table";
import { Text } from "@cloudflare/kumo/components/text";
import { CodeIcon } from "@phosphor-icons/react";
import { useTranslation } from "react-i18next";

import { EmptyState, Loading, Page, Section, Time } from "../components/ui";
import { useAccount, useAccountQuery } from "../lib/hooks";
import type { Account, Bucket, D1Database, KvNamespace, Queue, Script, Workflow } from "../lib/types";

function Count({ label, href, count }: { label: string; href: string; count: number | undefined }) {
  return (
    <LayerCard>
      <LayerCard.Primary className="flex flex-col gap-1">
        <Link href={href} variant="plain" className="text-kumo-subtle">
          {label}
        </Link>
        <span className="text-2xl font-semibold">{count ?? "—"}</span>
      </LayerCard.Primary>
    </LayerCard>
  );
}

export function Overview() {
  const { t } = useTranslation();
  const account = useAccount();
  const base = `/accounts/${account}`;
  const info = useAccountQuery<Account>(["account"], "");
  const scripts = useAccountQuery<Script[]>(["scripts"], "/workers/scripts");
  const kv = useAccountQuery<KvNamespace[]>(["kv"], "/storage/kv/namespaces?per_page=100");
  const d1 = useAccountQuery<D1Database[]>(["d1"], "/d1/database");
  const r2 = useAccountQuery<{ buckets: Bucket[] }>(["r2"], "/r2/buckets");
  const queues = useAccountQuery<Queue[]>(["queues"], "/queues");
  const workflows = useAccountQuery<Workflow[]>(["workflows"], "/workflows");
  const subdomain = useAccountQuery<{ subdomain: string }>(["subdomain"], "/workers/subdomain");

  const recent = [...(scripts.data ?? [])].sort((a, b) => b.modified_on.localeCompare(a.modified_on)).slice(0, 5);

  return (
    <Page title={info.data?.name ?? t("nav.overview")} description={t("overview.description")}>
      <Grid variant="1-2-4up" gap="base">
        <Count label={t("nav.workers")} href={`${base}/workers`} count={scripts.data?.length} />
        <Count label={t("nav.kv")} href={`${base}/kv`} count={kv.data?.length} />
        <Count label={t("nav.d1")} href={`${base}/d1`} count={d1.data?.length} />
        <Count label={t("nav.r2")} href={`${base}/r2`} count={r2.data?.buckets.length} />
        <Count label={t("nav.queues")} href={`${base}/queues`} count={queues.data?.length} />
        <Count label={t("nav.workflows")} href={`${base}/workflows`} count={workflows.data?.length} />
      </Grid>
      <Section title={t("overview.accountDetails")}>
        <div className="flex flex-col gap-3">
          <div className="flex flex-col gap-1">
            <Text variant="secondary" size="sm">
              {t("overview.accountId")}
            </Text>
            <ClipboardText text={account} size="base" />
          </div>
          {subdomain.data && (
            <div className="flex flex-col gap-1">
              <Text variant="secondary" size="sm">
                {t("overview.subdomain")}
              </Text>
              <ClipboardText text={subdomain.data.subdomain} size="base" />
            </div>
          )}
        </div>
      </Section>
      <Section title={t("overview.recentDeployments")} flush>
        {scripts.isPending ? (
          <Loading />
        ) : recent.length === 0 ? (
          <EmptyState icon={<CodeIcon size={32} />} title={t("workers.empty")} description={t("workers.emptyHelp")} />
        ) : (
          <Table>
            <Table.Header>
              <Table.Row>
                <Table.Head>{t("common.name")}</Table.Head>
                <Table.Head>{t("workers.lastDeployed")}</Table.Head>
              </Table.Row>
            </Table.Header>
            <Table.Body>
              {recent.map((s) => (
                <Table.Row key={s.id}>
                  <Table.Cell>
                    <Link href={`${base}/workers/${encodeURIComponent(s.id)}`}>{s.id}</Link>
                  </Table.Cell>
                  <Table.Cell>
                    <Time value={s.modified_on} />
                  </Table.Cell>
                </Table.Row>
              ))}
            </Table.Body>
          </Table>
        )}
      </Section>
    </Page>
  );
}
