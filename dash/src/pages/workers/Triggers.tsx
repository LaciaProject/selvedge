import { Button } from "@cloudflare/kumo/components/button";
import { ClipboardText } from "@cloudflare/kumo/components/clipboard-text";
import { Input } from "@cloudflare/kumo/components/input";
import { Switch } from "@cloudflare/kumo/components/switch";
import { Table } from "@cloudflare/kumo/components/table";
import { PlusIcon, TrashIcon } from "@phosphor-icons/react";
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { ConfirmDelete, FormDialog } from "../../components/dialogs";
import { Async, EmptyState, Mono, Section, Time } from "../../components/ui";
import { useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import { authConfigQuery } from "../../lib/session";
import type { Domain, Route, Schedule } from "../../lib/types";

/** A one-field "add" dialog. */
function AddOne(props: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  title: string;
  description: string;
  label: string;
  placeholder: string;
  valid: (v: string) => boolean;
  onAdd: (v: string) => Promise<unknown>;
}) {
  const { t } = useTranslation();
  const [value, setValue] = useState("");
  return (
    <FormDialog
      open={props.open}
      onOpenChange={(o) => {
        props.onOpenChange(o);
        if (!o) setValue("");
      }}
      title={props.title}
      description={props.description}
      submitLabel={t("common.add")}
      disabled={!props.valid(value.trim())}
      onSubmit={async () => {
        await props.onAdd(value.trim());
      }}
    >
      <Input label={props.label} placeholder={props.placeholder} value={value} onChange={(e) => setValue(e.target.value)} autoComplete="off" />
    </FormDialog>
  );
}

function RemoveButton({ label, onClick }: { label: string; onClick: () => void }) {
  return (
    <Button variant="ghost" size="sm" shape="square" aria-label={label} onClick={onClick}>
      <TrashIcon size={16} />
    </Button>
  );
}

export function Triggers({ script }: { script: string }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const path = `/workers/scripts/${encodeURIComponent(script)}`;
  const config = useQuery(authConfigQuery).data;
  const subdomain = useAccountQuery<{ subdomain: string }>(["subdomain"], "/workers/subdomain");
  const devState = useAccountQuery<{ enabled: boolean }>(["workers-dev", script], `${path}/subdomain`);
  const routes = useAccountQuery<Route[]>(["routes", script], `${path}/routes`);
  const domains = useAccountQuery<Domain[]>(["domains"], "/workers/domains");
  const schedules = useAccountQuery<{ schedules: Schedule[] }>(["schedules", script], `${path}/schedules`);
  const [dialog, setDialog] = useState<"route" | "domain" | "cron">();
  const [removing, setRemoving] = useState<{ kind: string; name: string; run: () => Promise<unknown> }>();

  const toggleDev = useChange((enabled: boolean) => call(`${path}/subdomain`, { method: "POST", json: { enabled } }), {
    done: (_, on) => (on ? t("workers.devEnabled") : t("workers.devDisabled")),
    invalidate: [["workers-dev", script]],
  });
  const putRoutes = useChange(
    (patterns: string[]) => call(`${path}/routes`, { method: "PUT", json: patterns.map((pattern) => ({ pattern })) }),
    { invalidate: [["routes", script]], silentError: true },
  );
  const putCrons = useChange(
    (crons: string[]) => call(`${path}/schedules`, { method: "PUT", json: crons.map((cron) => ({ cron })) }),
    { invalidate: [["schedules", script]], silentError: true },
  );
  const addDomain = useChange(
    (hostname: string) => call("/workers/domains", { method: "PUT", json: { hostname, service: script, environment: "production" } }),
    { invalidate: [["domains"]], silentError: true },
  );
  const removeDomain = useChange((id: string) => call(`/workers/domains/${id}`, { method: "DELETE" }), {
    invalidate: [["domains"]],
    silentError: true,
  });

  const devUrl =
    subdomain.data && config?.workers_dev_zone ? `https://${script}.${subdomain.data.subdomain}.${config.workers_dev_zone}` : undefined;
  const mine = (domains.data ?? []).filter((d) => d.service === script);
  const patterns = (routes.data ?? []).map((r) => r.pattern);
  const crons = (schedules.data?.schedules ?? []).map((s) => s.cron);

  return (
    <div className="flex flex-col gap-6">
      <Section title={t("workers.workersDev")} description={t("workers.workersDevHelp")}>
        <Async query={devState}>
          {(d) => (
            <div className="flex flex-col gap-3">
              <Switch
                label={t("workers.workersDevEnabled")}
                checked={d.enabled}
                transitioning={toggleDev.isPending}
                onClick={() => toggleDev.mutate(!d.enabled)}
              />
              {d.enabled && devUrl && <ClipboardText text={devUrl} size="base" />}
            </div>
          )}
        </Async>
      </Section>

      <Section
        title={t("workers.routes")}
        description={t("workers.routesHelp")}
        actions={
          <Button size="sm" icon={PlusIcon} onClick={() => setDialog("route")}>
            {t("workers.addRoute")}
          </Button>
        }
        flush
      >
        <Async query={routes} empty={(r) => r.length === 0} emptyState={<EmptyState title={t("workers.noRoutes")} />}>
          {(r) => (
            <Table>
              <Table.Body>
                {r.map((route) => (
                  <Table.Row key={route.id}>
                    <Table.Cell>
                      <Mono>{route.pattern}</Mono>
                    </Table.Cell>
                    <Table.Cell className="w-px text-right whitespace-nowrap">
                      <RemoveButton
                        label={t("workers.removeRoute", { pattern: route.pattern })}
                        onClick={() =>
                          setRemoving({
                            kind: t("kinds.route"),
                            name: route.pattern,
                            run: () => putRoutes.mutateAsync(patterns.filter((p) => p !== route.pattern)),
                          })
                        }
                      />
                    </Table.Cell>
                  </Table.Row>
                ))}
              </Table.Body>
            </Table>
          )}
        </Async>
      </Section>

      <Section
        title={t("workers.domains")}
        description={t("workers.domainsHelp")}
        actions={
          <Button size="sm" icon={PlusIcon} onClick={() => setDialog("domain")}>
            {t("workers.addDomain")}
          </Button>
        }
        flush
      >
        <Async query={domains} empty={() => mine.length === 0} emptyState={<EmptyState title={t("workers.noDomains")} />}>
          {() => (
            <Table>
              <Table.Body>
                {mine.map((d) => (
                  <Table.Row key={d.id}>
                    <Table.Cell>
                      <Mono>{d.hostname}</Mono>
                    </Table.Cell>
                    <Table.Cell className="w-px text-right whitespace-nowrap">
                      <RemoveButton
                        label={t("workers.removeDomain", { hostname: d.hostname })}
                        onClick={() =>
                          setRemoving({ kind: t("kinds.domain"), name: d.hostname, run: () => removeDomain.mutateAsync(d.id) })
                        }
                      />
                    </Table.Cell>
                  </Table.Row>
                ))}
              </Table.Body>
            </Table>
          )}
        </Async>
      </Section>

      <Section
        title={t("workers.crons")}
        description={t("workers.cronsHelp")}
        actions={
          <Button size="sm" icon={PlusIcon} onClick={() => setDialog("cron")}>
            {t("workers.addCron")}
          </Button>
        }
        flush
      >
        <Async query={schedules} empty={(s) => s.schedules.length === 0} emptyState={<EmptyState title={t("workers.noCrons")} />}>
          {(s) => (
            <Table>
              <Table.Body>
                {s.schedules.map((c) => (
                  <Table.Row key={c.cron}>
                    <Table.Cell>
                      <Mono>{c.cron}</Mono>
                    </Table.Cell>
                    <Table.Cell className="text-kumo-subtle">
                      <Time value={c.created_on} />
                    </Table.Cell>
                    <Table.Cell className="w-px text-right whitespace-nowrap">
                      <RemoveButton
                        label={t("workers.removeCron", { cron: c.cron })}
                        onClick={() =>
                          setRemoving({
                            kind: t("kinds.cron"),
                            name: c.cron,
                            run: () => putCrons.mutateAsync(crons.filter((x) => x !== c.cron)),
                          })
                        }
                      />
                    </Table.Cell>
                  </Table.Row>
                ))}
              </Table.Body>
            </Table>
          )}
        </Async>
      </Section>

      <AddOne
        open={dialog === "route"}
        onOpenChange={(o) => !o && setDialog(undefined)}
        title={t("workers.addRoute")}
        description={t("workers.addRouteHelp")}
        label={t("workers.routePattern")}
        placeholder="example.com/api/*"
        valid={(v) => v.length > 2 && !v.includes(" ")}
        onAdd={(v) => putRoutes.mutateAsync([...patterns, v])}
      />
      <AddOne
        open={dialog === "domain"}
        onOpenChange={(o) => !o && setDialog(undefined)}
        title={t("workers.addDomain")}
        description={t("workers.addDomainHelp")}
        label={t("workers.hostname")}
        placeholder="api.example.com"
        valid={(v) => /^[a-z0-9.-]+\.[a-z]{2,}$/i.test(v)}
        onAdd={(v) => addDomain.mutateAsync(v)}
      />
      <AddOne
        open={dialog === "cron"}
        onOpenChange={(o) => !o && setDialog(undefined)}
        title={t("workers.addCron")}
        description={t("workers.addCronHelp")}
        label={t("workers.cronExpression")}
        placeholder="*/15 * * * *"
        valid={(v) => v.split(/\s+/).length >= 5}
        onAdd={(v) => putCrons.mutateAsync([...crons, v])}
      />
      <ConfirmDelete
        open={removing !== undefined}
        onOpenChange={(o) => !o && setRemoving(undefined)}
        kind={removing?.kind ?? ""}
        name={removing?.name ?? ""}
        confirmLabel={t("common.remove")}
        consequence={t("workers.removeTriggerConsequence")}
        onConfirm={async () => {
          await removing?.run();
        }}
      />
    </div>
  );
}
