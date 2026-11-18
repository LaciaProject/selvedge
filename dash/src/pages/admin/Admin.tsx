import { Badge } from "@cloudflare/kumo/components/badge";
import { Button } from "@cloudflare/kumo/components/button";
import { Input } from "@cloudflare/kumo/components/input";
import { Link } from "@cloudflare/kumo/components/link";
import { Select } from "@cloudflare/kumo/components/select";
import { Table } from "@cloudflare/kumo/components/table";
import { PlusIcon, TrashIcon } from "@phosphor-icons/react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useSearch } from "@tanstack/react-router";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { ConfirmDelete, FormDialog } from "../../components/dialogs";
import { UrlTabs } from "../../components/TabbedPage";
import { Async, EmptyState, Mono, Page, Section, Time } from "../../components/ui";
import { api } from "../../lib/api";
import { useChange } from "../../lib/hooks";
import type { Account, User, Zone } from "../../lib/types";

const accountsQuery = { queryKey: ["admin", "accounts"], queryFn: () => api<Account[]>("/client/v4/accounts") };

export function Admin() {
  const { t } = useTranslation();
  const search = useSearch({ strict: false }) as { tab?: string };
  return (
    <Page title={t("nav.admin")} description={t("admin.description")}>
      <UrlTabs
        current={search.tab}
        tabs={[
          { value: "accounts", label: t("admin.tabs.accounts"), render: () => <Accounts /> },
          { value: "users", label: t("admin.tabs.users"), render: () => <Users /> },
          { value: "zones", label: t("admin.tabs.zones"), render: () => <Zones /> },
        ]}
      />
    </Page>
  );
}

function Accounts() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const accounts = useQuery(accountsQuery);
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [removing, setRemoving] = useState<Account>();
  const refresh = async () => {
    await qc.invalidateQueries({ queryKey: ["admin", "accounts"] });
    await qc.invalidateQueries({ queryKey: ["me"] });
  };
  const create = useChange((name: string) => api<Account>("/client/v4/accounts", { method: "POST", json: { name } }), {
    done: (_, name) => t("admin.accountCreated", { name }),
    silentError: true,
  });
  const remove = useChange((id: string) => api(`/client/v4/accounts/${id}`, { method: "DELETE" }), { silentError: true });
  return (
    <Section
      title={t("admin.accounts")}
      actions={
        <Button size="sm" icon={PlusIcon} onClick={() => setCreating(true)}>
          {t("admin.createAccount")}
        </Button>
      }
      flush
    >
      <Async query={accounts} empty={(a) => a.length === 0} emptyState={<EmptyState title={t("admin.noAccounts")} />}>
        {(list) => (
          <Table>
            <Table.Header>
              <Table.Row>
                <Table.Head>{t("common.name")}</Table.Head>
                <Table.Head>{t("common.id")}</Table.Head>
                <Table.Head />
              </Table.Row>
            </Table.Header>
            <Table.Body>
              {list.map((a) => (
                <Table.Row key={a.id}>
                  <Table.Cell>
                    <Link href={`/accounts/${a.id}`}>{a.name}</Link>
                  </Table.Cell>
                  <Table.Cell>
                    <Mono className="text-kumo-subtle">{a.id}</Mono>
                  </Table.Cell>
                  <Table.Cell className="w-px text-right whitespace-nowrap">
                    <Button variant="ghost" size="sm" shape="square" aria-label={t("admin.deleteAccount", { name: a.name })} onClick={() => setRemoving(a)}>
                      <TrashIcon size={16} />
                    </Button>
                  </Table.Cell>
                </Table.Row>
              ))}
            </Table.Body>
          </Table>
        )}
      </Async>
      <FormDialog
        open={creating}
        onOpenChange={(o) => {
          setCreating(o);
          if (!o) setName("");
        }}
        title={t("admin.createAccount")}
        submitLabel={t("common.create")}
        disabled={!name.trim()}
        onSubmit={async () => {
          await create.mutateAsync(name.trim());
          await refresh();
        }}
      >
        <Input label={t("common.name")} value={name} onChange={(e) => setName(e.target.value)} autoComplete="off" />
      </FormDialog>
      <ConfirmDelete
        open={removing !== undefined}
        onOpenChange={(o) => !o && setRemoving(undefined)}
        kind={t("kinds.account")}
        name={removing?.name ?? ""}
        consequence={t("settings.deleteAccountConsequence", { name: removing?.name })}
        onConfirm={async () => {
          await remove.mutateAsync(removing!.id);
          await refresh();
        }}
      />
    </Section>
  );
}

function Users() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const users = useQuery({ queryKey: ["admin", "users"], queryFn: () => api<User[]>("/client/v4/selvedge/users") });
  const [removing, setRemoving] = useState<User>();
  const remove = useChange((id: string) => api(`/client/v4/selvedge/users/${id}`, { method: "DELETE" }), { silentError: true });
  return (
    <Section title={t("admin.users")} description={t("admin.usersHelp")} flush>
      <Async query={users} empty={(u) => u.length === 0} emptyState={<EmptyState title={t("admin.noUsers")} />}>
        {(list) => (
          <Table>
            <Table.Header>
              <Table.Row>
                <Table.Head>{t("common.name")}</Table.Head>
                <Table.Head>{t("settings.email")}</Table.Head>
                <Table.Head>{t("settings.lastLogin")}</Table.Head>
                <Table.Head />
              </Table.Row>
            </Table.Header>
            <Table.Body>
              {list.map((u) => (
                <Table.Row key={u.id}>
                  <Table.Cell>
                    <span className="flex items-center gap-2">
                      {u.name ?? "—"}
                      {u.admin && <Badge variant="info">{t("admin.admin")}</Badge>}
                    </span>
                  </Table.Cell>
                  <Table.Cell>{u.email ?? "—"}</Table.Cell>
                  <Table.Cell>
                    <Time value={u.last_login_on} />
                  </Table.Cell>
                  <Table.Cell className="w-px text-right whitespace-nowrap">
                    <Button variant="ghost" size="sm" shape="square" aria-label={t("admin.removeUser", { name: u.email ?? u.id })} onClick={() => setRemoving(u)}>
                      <TrashIcon size={16} />
                    </Button>
                  </Table.Cell>
                </Table.Row>
              ))}
            </Table.Body>
          </Table>
        )}
      </Async>
      <ConfirmDelete
        open={removing !== undefined}
        onOpenChange={(o) => !o && setRemoving(undefined)}
        kind={t("kinds.user")}
        name={removing?.email ?? removing?.id ?? ""}
        confirmLabel={t("common.remove")}
        consequence={t("admin.removeUserConsequence")}
        onConfirm={async () => {
          await remove.mutateAsync(removing!.id);
          await qc.invalidateQueries({ queryKey: ["admin", "users"] });
        }}
      />
    </Section>
  );
}

function Zones() {
  const { t } = useTranslation();
  const qc = useQueryClient();
  const zones = useQuery({ queryKey: ["admin", "zones"], queryFn: () => api<Zone[]>("/client/v4/zones") });
  const accounts = useQuery(accountsQuery);
  const names = new Map((accounts.data ?? []).map((a) => [a.id, a.name]));
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [owner, setOwner] = useState("");
  const [removing, setRemoving] = useState<Zone>();
  const refresh = () => qc.invalidateQueries({ queryKey: ["admin", "zones"] });
  const create = useChange(
    () => api<Zone>("/client/v4/zones", { method: "POST", json: { name: name.trim(), account: { id: owner } } }),
    { done: t("admin.zoneCreated", { name: name.trim() }), silentError: true },
  );
  const remove = useChange((id: string) => api(`/client/v4/zones/${id}`, { method: "DELETE" }), { silentError: true });
  return (
    <Section
      title={t("admin.zones")}
      description={t("admin.zonesHelp")}
      actions={
        <Button size="sm" icon={PlusIcon} onClick={() => setCreating(true)}>
          {t("admin.addZone")}
        </Button>
      }
      flush
    >
      <Async query={zones} empty={(z) => z.length === 0} emptyState={<EmptyState title={t("admin.noZones")} />}>
        {(list) => (
          <Table>
            <Table.Header>
              <Table.Row>
                <Table.Head>{t("admin.zone")}</Table.Head>
                <Table.Head>{t("admin.account")}</Table.Head>
                <Table.Head />
              </Table.Row>
            </Table.Header>
            <Table.Body>
              {list.map((z) => (
                <Table.Row key={z.id}>
                  <Table.Cell>
                    <Mono>{z.name}</Mono>
                  </Table.Cell>
                  <Table.Cell>
                    <Link href={`/accounts/${z.account.id}`}>{names.get(z.account.id) ?? z.account.id}</Link>
                  </Table.Cell>
                  <Table.Cell className="w-px text-right whitespace-nowrap">
                    <Button variant="ghost" size="sm" shape="square" aria-label={t("admin.removeZone", { name: z.name })} onClick={() => setRemoving(z)}>
                      <TrashIcon size={16} />
                    </Button>
                  </Table.Cell>
                </Table.Row>
              ))}
            </Table.Body>
          </Table>
        )}
      </Async>
      <FormDialog
        open={creating}
        onOpenChange={(o) => {
          setCreating(o);
          if (!o) setName("");
        }}
        title={t("admin.addZone")}
        description={t("admin.addZoneHelp")}
        submitLabel={t("common.add")}
        disabled={!name.trim() || !owner}
        onSubmit={async () => {
          await create.mutateAsync();
          await refresh();
        }}
      >
        <Input label={t("admin.zone")} placeholder="example.com" value={name} onChange={(e) => setName(e.target.value)} autoComplete="off" />
        <Select
          label={t("admin.account")}
          hideLabel={false}
          value={owner}
          onValueChange={(v) => setOwner(String(v))}
          items={Object.fromEntries((accounts.data ?? []).map((a) => [a.id, a.name]))}
        >
          {(accounts.data ?? []).map((a) => (
            <Select.Option key={a.id} value={a.id}>
              {a.name}
            </Select.Option>
          ))}
        </Select>
      </FormDialog>
      <ConfirmDelete
        open={removing !== undefined}
        onOpenChange={(o) => !o && setRemoving(undefined)}
        kind={t("kinds.zone")}
        name={removing?.name ?? ""}
        consequence={t("admin.removeZoneConsequence")}
        onConfirm={async () => {
          await remove.mutateAsync(removing!.id);
          await refresh();
        }}
      />
    </Section>
  );
}
