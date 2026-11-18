import { Badge } from "@cloudflare/kumo/components/badge";
import { Button } from "@cloudflare/kumo/components/button";
import { DropdownMenu } from "@cloudflare/kumo/components/dropdown";
import { Input } from "@cloudflare/kumo/components/input";
import { Meter } from "@cloudflare/kumo/components/meter";
import { Select } from "@cloudflare/kumo/components/select";
import { Table } from "@cloudflare/kumo/components/table";
import { ArrowCounterClockwiseIcon, DotsThreeIcon, GitForkIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { FormDialog } from "../../components/dialogs";
import { Async, Mono, Section, Time } from "../../components/ui";
import { useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import type { Deployment, Version } from "../../lib/types";

const short = (id: string) => id.slice(0, 8);

export function Deployments({ script }: { script: string }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const path = `/workers/scripts/${encodeURIComponent(script)}`;
  const versions = useAccountQuery<{ items: Version[] }>(["versions", script], `${path}/versions`);
  const deployments = useAccountQuery<{ deployments: Deployment[] }>(["deployments", script], `${path}/deployments`);
  const [split, setSplit] = useState<Version>();

  const deploy = useChange(
    (versions: { version_id: string; percentage: number }[]) =>
      call(`${path}/deployments`, { method: "POST", json: { strategy: "percentage", versions } }),
    {
      done: t("workers.deployed"),
      invalidate: [["deployments", script], ["scripts"]],
    },
  );

  const current = deployments.data?.deployments[0];
  const live = new Map(current?.versions.map((v) => [v.version_id, v.percentage]) ?? []);
  const numberOf = new Map(versions.data?.items.map((v) => [v.id, v.number]) ?? []);

  return (
    <div className="flex flex-col gap-6">
      <Section title={t("workers.currentDeployment")}>
        <Async query={deployments}>
          {(d) =>
            current ? (
              <div className="flex flex-col gap-3">
                {current.versions.map((v) => (
                  <Meter
                    key={v.version_id}
                    label={`${t("workers.versionNumber", { number: numberOf.get(v.version_id) ?? "?" })} · ${short(v.version_id)}`}
                    value={v.percentage}
                    showValue
                    customValue={t("workers.percent", { value: v.percentage })}
                  />
                ))}
                <span className="text-sm text-kumo-subtle">
                  {t("workers.deployedAt")} <Time value={current.created_on} />
                  {d.deployments.length > 1 && ` · ${t("workers.deploymentCount", { count: d.deployments.length })}`}
                </span>
              </div>
            ) : (
              <span className="text-kumo-subtle">{t("workers.noDeployment")}</span>
            )
          }
        </Async>
      </Section>
      <Section title={t("workers.versions")} description={t("workers.versionsHelp")} flush>
        <Async query={versions}>
          {(v) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("workers.version")}</Table.Head>
                  <Table.Head>{t("common.id")}</Table.Head>
                  <Table.Head>{t("common.created")}</Table.Head>
                  <Table.Head>{t("workers.source")}</Table.Head>
                  <Table.Head />
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {v.items.map((ver) => (
                  <Table.Row key={ver.id}>
                    <Table.Cell>
                      <span className="flex items-center gap-2">
                        {ver.number}
                        {live.has(ver.id) && (
                          <Badge variant="success">{t("workers.livePercent", { value: live.get(ver.id) })}</Badge>
                        )}
                      </span>
                    </Table.Cell>
                    <Table.Cell>
                      <Mono>{short(ver.id)}</Mono>
                    </Table.Cell>
                    <Table.Cell>
                      <Time value={ver.metadata.created_on} />
                    </Table.Cell>
                    <Table.Cell>{ver.annotations?.["workers/triggered_by"] ?? ver.metadata.source}</Table.Cell>
                    <Table.Cell className="w-px text-right whitespace-nowrap">
                      <DropdownMenu>
                        <DropdownMenu.Trigger
                          render={
                            <Button variant="ghost" size="sm" shape="square" aria-label={t("common.actions")}>
                              <DotsThreeIcon size={18} />
                            </Button>
                          }
                        />
                        <DropdownMenu.Content>
                          <DropdownMenu.Item
                            icon={ArrowCounterClockwiseIcon}
                            onClick={() => deploy.mutate([{ version_id: ver.id, percentage: 100 }])}
                          >
                            {t("workers.deployThis")}
                          </DropdownMenu.Item>
                          <DropdownMenu.Item icon={GitForkIcon} onClick={() => setSplit(ver)}>
                            {t("workers.splitTraffic")}
                          </DropdownMenu.Item>
                        </DropdownMenu.Content>
                      </DropdownMenu>
                    </Table.Cell>
                  </Table.Row>
                ))}
              </Table.Body>
            </Table>
          )}
        </Async>
      </Section>
      {split && versions.data && (
        <SplitDialog
          versions={versions.data.items}
          first={split}
          onClose={() => setSplit(undefined)}
          onDeploy={(v) => deploy.mutateAsync(v)}
        />
      )}
    </div>
  );
}

function SplitDialog(props: {
  versions: Version[];
  first: Version;
  onClose: () => void;
  onDeploy: (v: { version_id: string; percentage: number }[]) => Promise<unknown>;
}) {
  const { t } = useTranslation();
  const others = props.versions.filter((v) => v.id !== props.first.id);
  const [second, setSecond] = useState(others[0]?.id ?? "");
  const [percent, setPercent] = useState("10");
  const p = Number(percent);
  const valid = second !== "" && Number.isFinite(p) && p > 0 && p < 100;
  return (
    <FormDialog
      open
      onOpenChange={(o) => !o && props.onClose()}
      title={t("workers.splitTraffic")}
      description={t("workers.splitHelp")}
      submitLabel={t("workers.deploy")}
      disabled={!valid}
      onSubmit={async () => {
        await props.onDeploy([
          { version_id: props.first.id, percentage: p },
          { version_id: second, percentage: 100 - p },
        ]);
      }}
    >
      <Input
        label={t("workers.percentFor", { number: props.first.number })}
        type="number"
        min={1}
        max={99}
        value={percent}
        onChange={(e) => setPercent(e.target.value)}
      />
      <Select
        label={t("workers.restGoesTo")}
        hideLabel={false}
        value={second}
        onValueChange={(v) => setSecond(String(v))}
        items={Object.fromEntries(others.map((v) => [v.id, t("workers.versionNumber", { number: v.number })]))}
      >
        {others.map((v) => (
          <Select.Option key={v.id} value={v.id}>
            {t("workers.versionNumber", { number: v.number })}
          </Select.Option>
        ))}
      </Select>
    </FormDialog>
  );
}
