import { Badge } from "@cloudflare/kumo/components/badge";
import { Button } from "@cloudflare/kumo/components/button";
import { Input, InputArea } from "@cloudflare/kumo/components/input";
import { Link } from "@cloudflare/kumo/components/link";
import { Table } from "@cloudflare/kumo/components/table";
import { PlusIcon, TrashIcon } from "@phosphor-icons/react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { ConfirmDelete, FormDialog } from "../../components/dialogs";
import { Async, EmptyState, Facts, Mono, Section } from "../../components/ui";
import { useAccount, useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import type { Binding, ScriptSettings, Version } from "../../lib/types";

/** What a binding points at, linked where the dashboard has a page for it. */
function BindingTarget({ b }: { b: Binding }) {
  const { t } = useTranslation();
  const account = useAccount();
  const base = `/accounts/${account}`;
  const s = (k: string) => (typeof b[k] === "string" ? (b[k] as string) : undefined);
  switch (b.type) {
    case "plain_text":
      return <Mono>{s("text")}</Mono>;
    case "secret_text":
      return <span className="text-kumo-subtle">{t("workers.secretHidden")}</span>;
    case "kv_namespace":
      return <Link href={`${base}/kv/${s("namespace_id")}`}>{s("namespace_id")}</Link>;
    case "d1":
      return <Link href={`${base}/d1/${s("id") ?? s("database_id")}`}>{s("id") ?? s("database_id")}</Link>;
    case "r2_bucket":
      return <Link href={`${base}/r2/${encodeURIComponent(s("bucket_name") ?? "")}`}>{s("bucket_name")}</Link>;
    case "queue":
      return <Link href={`${base}/queues`}>{s("queue_name")}</Link>;
    case "durable_object_namespace":
      return (
        <span>
          {s("class_name")}
          {s("script_name") && <span className="text-kumo-subtle"> ({s("script_name")})</span>}
        </span>
      );
    case "service":
      return <Link href={`${base}/workers/${encodeURIComponent(s("service") ?? "")}`}>{s("service")}</Link>;
    case "workflow":
      return <Link href={`${base}/workflows/${encodeURIComponent(s("workflow_name") ?? "")}`}>{s("workflow_name")}</Link>;
    default: {
      const rest = Object.fromEntries(Object.entries(b).filter(([k]) => k !== "name" && k !== "type"));
      return <Mono className="text-kumo-subtle">{Object.keys(rest).length ? JSON.stringify(rest) : ""}</Mono>;
    }
  }
}

export function Settings({ script }: { script: string }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const path = `/workers/scripts/${encodeURIComponent(script)}`;
  const settings = useAccountQuery<ScriptSettings>(["settings", script], `${path}/settings`);
  const versions = useAccountQuery<{ items: Version[] }>(["versions", script], `${path}/versions`);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<string>();
  const latest = versions.data?.items[0];

  const removeSecret = useChange(
    (name: string) => call(`${path}/secrets/${encodeURIComponent(name)}`, { method: "DELETE" }),
    { done: (_, name) => t("workers.secretDeleted", { name }), invalidate: [["settings", script], ["versions", script]], silentError: true },
  );

  return (
    <div className="flex flex-col gap-6">
      <Section title={t("workers.bindings")} description={t("workers.bindingsHelp")} flush>
        <Async
          query={settings}
          empty={(s) => s.bindings.filter((b) => b.type !== "secret_text").length === 0}
          emptyState={<EmptyState title={t("workers.noBindings")} />}
        >
          {(s) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("common.name")}</Table.Head>
                  <Table.Head>{t("common.type")}</Table.Head>
                  <Table.Head>{t("workers.target")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {s.bindings
                  .filter((b) => b.type !== "secret_text")
                  .map((b) => (
                    <Table.Row key={b.name}>
                      <Table.Cell>
                        <Mono>{b.name}</Mono>
                      </Table.Cell>
                      <Table.Cell>
                        <Badge variant="neutral">{t(`bindingTypes.${b.type}`, { defaultValue: b.type })}</Badge>
                      </Table.Cell>
                      <Table.Cell>
                        <BindingTarget b={b} />
                      </Table.Cell>
                    </Table.Row>
                  ))}
              </Table.Body>
            </Table>
          )}
        </Async>
      </Section>
      <Section
        title={t("workers.secrets")}
        description={t("workers.secretsHelp")}
        actions={
          <Button size="sm" icon={PlusIcon} onClick={() => setAdding(true)}>
            {t("workers.addSecret")}
          </Button>
        }
        flush
      >
        <Async
          query={settings}
          empty={(s) => !s.bindings.some((b) => b.type === "secret_text")}
          emptyState={<EmptyState title={t("workers.noSecrets")} />}
        >
          {(s) => (
            <Table>
              <Table.Body>
                {s.bindings
                  .filter((b) => b.type === "secret_text")
                  .map((b) => (
                    <Table.Row key={b.name}>
                      <Table.Cell>
                        <Mono>{b.name}</Mono>
                      </Table.Cell>
                      <Table.Cell className="w-px text-right whitespace-nowrap">
                        <Button
                          variant="ghost"
                          size="sm"
                          shape="square"
                          aria-label={t("workers.deleteSecret", { name: b.name })}
                          onClick={() => setRemoving(b.name)}
                        >
                          <TrashIcon size={16} />
                        </Button>
                      </Table.Cell>
                    </Table.Row>
                  ))}
              </Table.Body>
            </Table>
          )}
        </Async>
      </Section>
      <Section title={t("workers.runtime")}>
        <Async query={versions}>
          {() => (
            <Facts
              items={[
                [t("workers.compatibilityDate"), <Mono key="d">{latest?.resources.script_runtime.compatibility_date ?? "—"}</Mono>],
                [
                  t("workers.compatibilityFlags"),
                  <span key="f" className="flex flex-wrap gap-1">
                    {latest?.resources.script_runtime.compatibility_flags.length
                      ? latest.resources.script_runtime.compatibility_flags.map((f) => (
                          <Badge key={f} variant="neutral">
                            {f}
                          </Badge>
                        ))
                      : "—"}
                  </span>,
                ],
              ]}
            />
          )}
        </Async>
      </Section>
      <AddSecret script={script} open={adding} onOpenChange={setAdding} />
      <ConfirmDelete
        open={removing !== undefined}
        onOpenChange={(o) => !o && setRemoving(undefined)}
        kind={t("kinds.secret")}
        name={removing ?? ""}
        onConfirm={async () => {
          await removeSecret.mutateAsync(removing!);
        }}
      />
    </div>
  );
}

function AddSecret({ script, open, onOpenChange }: { script: string; open: boolean; onOpenChange: (o: boolean) => void }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const put = useChange(
    () =>
      call(`/workers/scripts/${encodeURIComponent(script)}/secrets`, {
        method: "PUT",
        json: { name: name.trim(), text: value, type: "secret_text" },
      }),
    { done: t("workers.secretSaved", { name: name.trim() }), invalidate: [["settings", script], ["versions", script]], silentError: true },
  );
  return (
    <FormDialog
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) {
          setName("");
          setValue("");
        }
      }}
      title={t("workers.addSecret")}
      description={t("workers.addSecretHelp")}
      submitLabel={t("common.save")}
      disabled={!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name.trim()) || value === ""}
      onSubmit={async () => {
        await put.mutateAsync();
      }}
    >
      <Input label={t("common.name")} value={name} onChange={(e) => setName(e.target.value)} autoComplete="off" />
      <InputArea label={t("workers.secretValue")} value={value} onValueChange={setValue} rows={3} autoComplete="off" spellCheck={false} />
    </FormDialog>
  );
}
