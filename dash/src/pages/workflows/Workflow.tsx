import { Badge } from "@cloudflare/kumo/components/badge";
import { Button } from "@cloudflare/kumo/components/button";
import { DropdownMenu } from "@cloudflare/kumo/components/dropdown";
import { Input, InputArea } from "@cloudflare/kumo/components/input";
import { Pagination } from "@cloudflare/kumo/components/pagination";
import { Table } from "@cloudflare/kumo/components/table";
import { DotsThreeIcon, PlusIcon } from "@phosphor-icons/react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useParams } from "@tanstack/react-router";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Crumbs } from "../../components/Crumbs";
import { FormDialog } from "../../components/dialogs";
import { EmptyState, ErrorBanner, Loading, Mono, Page, Section, Time } from "../../components/ui";
import { accountPath, apiPage } from "../../lib/api";
import { useAccountApi, useChange } from "../../lib/hooks";
import type { WorkflowInstance } from "../../lib/types";

const PER_PAGE = 25;
const ACTIONS = ["pause", "resume", "terminate", "restart"] as const;

function statusVariant(s: string) {
  if (s === "complete") return "success" as const;
  if (s === "errored" || s === "terminated") return "error" as const;
  if (s === "paused" || s === "waiting") return "warning" as const;
  return "info" as const;
}

/** JSON text, or undefined if it is not JSON. Empty text is `{}`. */
function parseJson(text: string): { ok: boolean; value?: unknown } {
  if (!text.trim()) return { ok: true, value: {} };
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch {
    return { ok: false };
  }
}

export function Workflow() {
  const { t } = useTranslation();
  const { workflow } = useParams({ strict: false }) as { workflow: string };
  const { account, call } = useAccountApi();
  const [page, setPage] = useState(1);
  const base = `/workflows/${encodeURIComponent(workflow)}`;
  const instances = useQuery({
    queryKey: [account, "workflow-instances", workflow, page],
    queryFn: () => apiPage<WorkflowInstance[]>(accountPath(account, `${base}/instances`), { query: { page, per_page: PER_PAGE } }),
    placeholderData: keepPreviousData,
    refetchInterval: 10_000,
  });
  const [dialog, setDialog] = useState<{ kind: "create" } | { kind: "event"; id: string } | { kind: "details"; id: string }>();
  const act = useChange(
    ({ id, status }: { id: string; status: string }) =>
      call(`${base}/instances/${encodeURIComponent(id)}/status`, { method: "PATCH", json: { status } }),
    { done: (_, v) => t(`workflows.did.${v.status}`), invalidate: [["workflow-instances", workflow]] },
  );

  return (
    <Page
      crumbs={<Crumbs items={[{ label: t("nav.workflows"), href: `/accounts/${account}/workflows` }]} current={workflow} />}
      title={workflow}
      actions={
        <Button variant="primary" icon={PlusIcon} onClick={() => setDialog({ kind: "create" })}>
          {t("workflows.trigger")}
        </Button>
      }
    >
      <Section title={t("workflows.instances")} flush>
        {instances.isPending ? (
          <Loading />
        ) : instances.isError ? (
          <ErrorBanner error={instances.error} onRetry={() => instances.refetch()} />
        ) : instances.data.result.length === 0 ? (
          <EmptyState title={t("workflows.noInstances")} />
        ) : (
          <>
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("common.id")}</Table.Head>
                  <Table.Head>{t("workflows.status")}</Table.Head>
                  <Table.Head>{t("common.created")}</Table.Head>
                  <Table.Head />
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {instances.data.result.map((i) => (
                  <Table.Row key={i.id}>
                    <Table.Cell>
                      <button type="button" className="text-kumo-link hover:underline" onClick={() => setDialog({ kind: "details", id: i.id })}>
                        <Mono>{i.id}</Mono>
                      </button>
                    </Table.Cell>
                    <Table.Cell>
                      <Badge variant={statusVariant(i.status)}>{t(`workflows.statuses.${i.status}`, { defaultValue: i.status })}</Badge>
                    </Table.Cell>
                    <Table.Cell>
                      <Time value={i.created_on} />
                    </Table.Cell>
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
                          {ACTIONS.map((a) => (
                            <DropdownMenu.Item
                              key={a}
                              variant={a === "terminate" ? "danger" : "default"}
                              onClick={() => act.mutate({ id: i.id, status: a })}
                            >
                              {t(`workflows.actions.${a}`)}
                            </DropdownMenu.Item>
                          ))}
                          <DropdownMenu.Item onClick={() => setDialog({ kind: "event", id: i.id })}>
                            {t("workflows.sendEvent")}
                          </DropdownMenu.Item>
                        </DropdownMenu.Content>
                      </DropdownMenu>
                    </Table.Cell>
                  </Table.Row>
                ))}
              </Table.Body>
            </Table>
            <div className="border-t border-kumo-hairline p-2">
              <Pagination
                page={page}
                setPage={setPage}
                perPage={PER_PAGE}
                totalCount={instances.data.result_info?.total_count ?? instances.data.result.length}
                controls="simple"
              />
            </div>
          </>
        )}
      </Section>
      {dialog?.kind === "create" && <Trigger workflow={workflow} onClose={() => setDialog(undefined)} />}
      {dialog?.kind === "event" && <SendEvent workflow={workflow} instance={dialog.id} onClose={() => setDialog(undefined)} />}
      {dialog?.kind === "details" && <Details workflow={workflow} instance={dialog.id} onClose={() => setDialog(undefined)} />}
    </Page>
  );
}

function Trigger({ workflow, onClose }: { workflow: string; onClose: () => void }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const [id, setId] = useState("");
  const [params, setParams] = useState("{}");
  const parsed = parseJson(params);
  const create = useChange(
    () =>
      call(`/workflows/${encodeURIComponent(workflow)}/instances`, {
        method: "POST",
        json: { instance_id: id.trim() || undefined, params: parsed.value },
      }),
    { done: t("workflows.triggered"), invalidate: [["workflow-instances", workflow]], silentError: true },
  );
  return (
    <FormDialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={t("workflows.trigger")}
      submitLabel={t("workflows.triggerButton")}
      disabled={!parsed.ok}
      onSubmit={async () => {
        await create.mutateAsync();
      }}
    >
      <Input label={t("workflows.instanceId")} description={t("workflows.instanceIdHelp")} value={id} onChange={(e) => setId(e.target.value)} />
      <InputArea
        label={t("workflows.params")}
        value={params}
        onValueChange={setParams}
        rows={6}
        className="font-mono"
        spellCheck={false}
        error={parsed.ok ? undefined : t("queues.invalidJson")}
      />
    </FormDialog>
  );
}

function SendEvent({ workflow, instance, onClose }: { workflow: string; instance: string; onClose: () => void }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const [type, setType] = useState("");
  const [payload, setPayload] = useState("{}");
  const parsed = parseJson(payload);
  const send = useChange(
    () =>
      call(
        `/workflows/${encodeURIComponent(workflow)}/instances/${encodeURIComponent(instance)}/events/${encodeURIComponent(type.trim())}`,
        { method: "POST", json: parsed.value },
      ),
    { done: t("workflows.eventSent"), invalidate: [["workflow-instances", workflow]], silentError: true },
  );
  return (
    <FormDialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={t("workflows.sendEvent")}
      submitLabel={t("queues.sendButton")}
      disabled={!type.trim() || !parsed.ok}
      onSubmit={async () => {
        await send.mutateAsync();
      }}
    >
      <Input label={t("workflows.eventType")} value={type} onChange={(e) => setType(e.target.value)} />
      <InputArea
        label={t("workflows.payload")}
        value={payload}
        onValueChange={setPayload}
        rows={6}
        className="font-mono"
        spellCheck={false}
        error={parsed.ok ? undefined : t("queues.invalidJson")}
      />
    </FormDialog>
  );
}

function Details({ workflow, instance, onClose }: { workflow: string; instance: string; onClose: () => void }) {
  const { t } = useTranslation();
  const { account } = useAccountApi();
  const details = useQuery({
    queryKey: [account, "workflow-instance", workflow, instance],
    queryFn: () => apiPage<unknown>(accountPath(account, `/workflows/${encodeURIComponent(workflow)}/instances/${encodeURIComponent(instance)}`)),
  });
  return (
    <FormDialog open onOpenChange={(o) => !o && onClose()} title={instance} size="xl" submitLabel={t("common.close")} onSubmit={async () => {}}>
      {details.isPending && <Loading />}
      {details.isError && <ErrorBanner error={details.error} />}
      {details.data && (
        <pre className="max-h-[60vh] overflow-auto font-mono text-xs">{JSON.stringify(details.data.result, null, 2)}</pre>
      )}
    </FormDialog>
  );
}
