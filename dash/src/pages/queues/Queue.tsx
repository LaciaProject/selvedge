import { Button } from "@cloudflare/kumo/components/button";
import { InputArea } from "@cloudflare/kumo/components/input";
import { Radio } from "@cloudflare/kumo/components/radio";
import { Table } from "@cloudflare/kumo/components/table";
import { BroomIcon, PaperPlaneTiltIcon, TrashIcon } from "@phosphor-icons/react";
import { useNavigate, useParams } from "@tanstack/react-router";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Crumbs } from "../../components/Crumbs";
import { ConfirmDelete, FormDialog } from "../../components/dialogs";
import { Async, EmptyState, Facts, Mono, Page, Section, Time } from "../../components/ui";
import { useFormat } from "../../lib/format";
import { useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import type { Queue as QueueInfo } from "../../lib/types";

export function Queue() {
  const { t } = useTranslation();
  const f = useFormat();
  const { queue } = useParams({ strict: false }) as { queue: string };
  const { account, call } = useAccountApi();
  const navigate = useNavigate();
  const info = useAccountQuery<QueueInfo>(["queues", queue], `/queues/${queue}`, { refetchInterval: 10_000 });
  const [dialog, setDialog] = useState<"send" | "purge" | "delete">();
  const purge = useChange(() => call(`/queues/${queue}/purge`, { method: "POST", json: { delete_messages_permanently: true } }), {
    done: t("queues.purged"),
    invalidate: [["queues", queue]],
    silentError: true,
  });
  const remove = useChange(() => call(`/queues/${queue}`, { method: "DELETE" }), { invalidate: [["queues"]], silentError: true });
  const name = info.data?.queue_name ?? queue;

  return (
    <Page
      crumbs={<Crumbs items={[{ label: t("nav.queues"), href: `/accounts/${account}/queues` }]} current={name} />}
      title={name}
      actions={
        <>
          <Button icon={PaperPlaneTiltIcon} onClick={() => setDialog("send")}>
            {t("queues.send")}
          </Button>
          <Button variant="secondary-destructive" icon={BroomIcon} onClick={() => setDialog("purge")}>
            {t("queues.purge")}
          </Button>
          <Button variant="secondary-destructive" icon={TrashIcon} onClick={() => setDialog("delete")}>
            {t("common.delete")}
          </Button>
        </>
      }
    >
      <Async query={info}>
        {(q) => (
          <>
            <Section title={t("queues.details")}>
              <Facts
                items={[
                  [t("common.id"), <Mono key="id">{q.queue_id}</Mono>],
                  [t("queues.backlog"), f.number(q.message_backlog_count ?? 0)],
                  [t("queues.deliveryDelay"), t("common.seconds", { count: q.settings.delivery_delay ?? 0 })],
                  [t("queues.retention"), t("common.seconds", { count: q.settings.message_retention_period ?? 0 })],
                  [t("common.created"), <Time key="c" value={q.created_on} absolute />],
                ]}
              />
            </Section>
            <Section title={t("queues.consumers")} description={t("queues.consumersHelp")} flush>
              {q.consumers.length === 0 ? (
                <EmptyState title={t("queues.noConsumer")} />
              ) : (
                <Table>
                  <Table.Header>
                    <Table.Row>
                      <Table.Head>{t("queues.consumer")}</Table.Head>
                      <Table.Head>{t("queues.batchSize")}</Table.Head>
                      <Table.Head>{t("queues.maxRetries")}</Table.Head>
                      <Table.Head>{t("queues.maxWait")}</Table.Head>
                      <Table.Head>{t("queues.deadLetter")}</Table.Head>
                    </Table.Row>
                  </Table.Header>
                  <Table.Body>
                    {q.consumers.map((c) => (
                      <Table.Row key={c.consumer_id}>
                        <Table.Cell>{c.script_name ?? t("queues.httpPull")}</Table.Cell>
                        <Table.Cell>{c.settings.batch_size ?? "—"}</Table.Cell>
                        <Table.Cell>{c.settings.max_retries ?? "—"}</Table.Cell>
                        <Table.Cell>{c.settings.max_wait_time_ms !== undefined ? t("common.ms", { count: c.settings.max_wait_time_ms }) : "—"}</Table.Cell>
                        <Table.Cell>{c.dead_letter_queue ?? "—"}</Table.Cell>
                      </Table.Row>
                    ))}
                  </Table.Body>
                </Table>
              )}
            </Section>
          </>
        )}
      </Async>
      <SendMessage queue={queue} open={dialog === "send"} onOpenChange={(o) => !o && setDialog(undefined)} />
      <ConfirmDelete
        open={dialog === "purge"}
        onOpenChange={(o) => !o && setDialog(undefined)}
        kind={t("kinds.queue")}
        name={name}
        confirmLabel={t("queues.purge")}
        consequence={t("queues.purgeConsequence")}
        onConfirm={async () => {
          await purge.mutateAsync();
        }}
      />
      <ConfirmDelete
        open={dialog === "delete"}
        onOpenChange={(o) => !o && setDialog(undefined)}
        kind={t("kinds.queue")}
        name={name}
        onConfirm={async () => {
          await remove.mutateAsync();
          navigate({ to: "/accounts/$account/queues", params: { account } });
        }}
      />
    </Page>
  );
}

function SendMessage({ queue, open, onOpenChange }: { queue: string; open: boolean; onOpenChange: (o: boolean) => void }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const [kind, setKind] = useState<"json" | "text">("json");
  const [body, setBody] = useState('{ "hello": "world" }');
  let parsed: unknown = body;
  let valid = true;
  if (kind === "json") {
    try {
      parsed = JSON.parse(body);
    } catch {
      valid = false;
    }
  }
  const send = useChange(() => call(`/queues/${queue}/messages`, { method: "POST", json: { body: parsed, content_type: kind } }), {
    done: t("queues.sent"),
    invalidate: [["queues", queue]],
    silentError: true,
  });
  return (
    <FormDialog
      open={open}
      onOpenChange={onOpenChange}
      title={t("queues.send")}
      description={t("queues.sendHelp")}
      submitLabel={t("queues.sendButton")}
      disabled={!valid}
      onSubmit={async () => {
        await send.mutateAsync();
      }}
    >
      <Radio.Group legend={t("queues.format")} value={kind} onValueChange={(v) => setKind(v as "json" | "text")} orientation="horizontal">
        <Radio.Item value="json" label="JSON" />
        <Radio.Item value="text" label={t("queues.text")} />
      </Radio.Group>
      <InputArea
        aria-label={t("queues.body")}
        value={body}
        onValueChange={setBody}
        rows={6}
        spellCheck={false}
        className="font-mono"
        error={valid ? undefined : t("queues.invalidJson")}
      />
    </FormDialog>
  );
}
