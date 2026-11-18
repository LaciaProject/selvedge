import { Badge } from "@cloudflare/kumo/components/badge";
import { Banner } from "@cloudflare/kumo/components/banner";
import { Button } from "@cloudflare/kumo/components/button";
import { ClipboardText } from "@cloudflare/kumo/components/clipboard-text";
import { Code } from "@cloudflare/kumo/components/code";
import { Input } from "@cloudflare/kumo/components/input";
import { Table } from "@cloudflare/kumo/components/table";
import { KeyIcon, PlusIcon, TrashIcon, UserPlusIcon, WarningIcon } from "@phosphor-icons/react";
import { useQuery } from "@tanstack/react-query";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { ConfirmDelete, FormDialog } from "../../components/dialogs";
import { UrlTabs } from "../../components/TabbedPage";
import { Async, EmptyState, Page, Section, Time } from "../../components/ui";
import { useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import { meQuery, queryClient } from "../../lib/session";
import type { Account, Member, NewToken, Token, User } from "../../lib/types";

export function Settings() {
  const { t } = useTranslation();
  const search = useSearch({ strict: false }) as { tab?: string };
  const me = useQuery(meQuery).data;
  const info = useAccountQuery<Account>(["account"], "");
  const tabs = [
    { value: "members", label: t("settings.tabs.members"), render: () => <Members admin={!!me?.admin} /> },
    { value: "tokens", label: t("settings.tabs.tokens"), render: () => <Tokens /> },
  ];
  if (me?.admin) tabs.push({ value: "danger", label: t("settings.tabs.danger"), render: () => <Danger name={info.data?.name} /> });
  return (
    <Page title={t("nav.settings")} description={info.data?.name}>
      <UrlTabs current={search.tab} tabs={tabs} />
    </Page>
  );
}

function who(u: User | null | undefined, fallback = "—") {
  return u ? (u.name ?? u.email ?? u.id) : fallback;
}

function Members({ admin }: { admin: boolean }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const members = useAccountQuery<Member[]>(["members"], "/selvedge/members");
  const group = useAccountQuery<{ group: string | null }>(["oidc-group"], "/selvedge/oidc-group");
  const [inviting, setInviting] = useState(false);
  const [editingGroup, setEditingGroup] = useState(false);
  const [removing, setRemoving] = useState<Member>();
  const remove = useChange((id: string) => call(`/selvedge/members/${id}`, { method: "DELETE" }), {
    invalidate: [["members"]],
    silentError: true,
  });
  return (
    <div className="flex flex-col gap-6">
      <Section
        title={t("settings.members")}
        description={t("settings.membersHelp")}
        actions={
          admin && (
            <Button size="sm" icon={UserPlusIcon} onClick={() => setInviting(true)}>
              {t("settings.invite")}
            </Button>
          )
        }
        flush
      >
        <Async query={members} empty={(m) => m.length === 0} emptyState={<EmptyState title={t("settings.noMembers")} />}>
          {(list) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("settings.person")}</Table.Head>
                  <Table.Head>{t("settings.email")}</Table.Head>
                  <Table.Head>{t("settings.lastLogin")}</Table.Head>
                  {admin && <Table.Head />}
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {list.map((m) => (
                  <Table.Row key={m.id}>
                    <Table.Cell>
                      {m.user ? who(m.user) : <Badge variant="warning">{t("settings.invited")}</Badge>}
                    </Table.Cell>
                    <Table.Cell>{m.user?.email ?? m.email}</Table.Cell>
                    <Table.Cell>{m.user ? <Time value={m.user.last_login_on} /> : "—"}</Table.Cell>
                    {admin && (
                      <Table.Cell className="w-px text-right whitespace-nowrap">
                        <Button
                          variant="ghost"
                          size="sm"
                          shape="square"
                          aria-label={t("settings.removeMember", { name: m.user?.email ?? m.email })}
                          onClick={() => setRemoving(m)}
                        >
                          <TrashIcon size={16} />
                        </Button>
                      </Table.Cell>
                    )}
                  </Table.Row>
                ))}
              </Table.Body>
            </Table>
          )}
        </Async>
      </Section>
      <Section
        title={t("settings.group")}
        description={t("settings.groupHelp")}
        actions={
          admin && (
            <Button size="sm" onClick={() => setEditingGroup(true)}>
              {t("common.edit")}
            </Button>
          )
        }
      >
        <Async query={group}>
          {(g) => (g.group ? <Badge variant="neutral">{g.group}</Badge> : <span className="text-kumo-subtle">{t("settings.noGroup")}</span>)}
        </Async>
      </Section>
      <Invite open={inviting} onOpenChange={setInviting} />
      {editingGroup && <EditGroup current={group.data?.group ?? ""} onClose={() => setEditingGroup(false)} />}
      <ConfirmDelete
        open={removing !== undefined}
        onOpenChange={(o) => !o && setRemoving(undefined)}
        kind={t("kinds.member")}
        name={removing?.user?.email ?? removing?.email ?? removing?.user?.id ?? ""}
        confirmLabel={t("common.remove")}
        consequence={t("settings.removeMemberConsequence")}
        onConfirm={async () => {
          await remove.mutateAsync(removing!.id);
        }}
      />
    </div>
  );
}

function Invite({ open, onOpenChange }: { open: boolean; onOpenChange: (o: boolean) => void }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const [email, setEmail] = useState("");
  const add = useChange(() => call("/selvedge/members", { method: "POST", json: { email: email.trim() } }), {
    done: t("settings.invitedDone", { email: email.trim() }),
    invalidate: [["members"]],
    silentError: true,
  });
  return (
    <FormDialog
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) setEmail("");
      }}
      title={t("settings.invite")}
      description={t("settings.inviteHelp")}
      submitLabel={t("settings.inviteButton")}
      disabled={!/^\S+@\S+\.\S+$/.test(email.trim())}
      onSubmit={async () => {
        await add.mutateAsync();
      }}
    >
      <Input label={t("settings.email")} type="email" value={email} onChange={(e) => setEmail(e.target.value)} autoComplete="off" />
    </FormDialog>
  );
}

function EditGroup({ current, onClose }: { current: string; onClose: () => void }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const [group, setGroup] = useState(current);
  const save = useChange(() => call("/selvedge/oidc-group", { method: "PUT", json: { group: group.trim() || null } }), {
    done: t("settings.groupSaved"),
    invalidate: [["oidc-group"]],
    silentError: true,
  });
  return (
    <FormDialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={t("settings.group")}
      description={t("settings.groupEditHelp")}
      submitLabel={t("common.save")}
      onSubmit={async () => {
        await save.mutateAsync();
      }}
    >
      <Input label={t("settings.groupName")} value={group} onChange={(e) => setGroup(e.target.value)} autoComplete="off" />
    </FormDialog>
  );
}

function Tokens() {
  const { t } = useTranslation();
  const me = useQuery(meQuery).data;
  // An account token does not make more of itself (RFC 0003 §6).
  if (me && !me.admin && !me.user) {
    return (
      <Section title={t("settings.tokens")} description={t("settings.tokensHelp")}>
        <EmptyState icon={<KeyIcon size={32} />} title={t("settings.tokensNeedPerson")} />
      </Section>
    );
  }
  return <TokenList />;
}

function TokenList() {
  const { t } = useTranslation();
  const { account, call } = useAccountApi();
  const tokens = useAccountQuery<Token[]>(["tokens"], "/selvedge/tokens");
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<NewToken>();
  const [revoking, setRevoking] = useState<Token>();
  const revoke = useChange((id: string) => call(`/selvedge/tokens/${id}`, { method: "DELETE" }), {
    invalidate: [["tokens"]],
    silentError: true,
  });
  return (
    <div className="flex flex-col gap-6">
      {created && (
        <Banner
          variant="alert"
          icon={<WarningIcon weight="fill" />}
          title={t("settings.tokenCreated", { name: created.name })}
          description={
            <div className="flex flex-col gap-3 pt-2">
              <span>{t("settings.tokenOnce")}</span>
              <ClipboardText text={created.value} size="base" />
              <span>{t("settings.wranglerSetup")}</span>
              <Code
                lang="bash"
                code={`export CLOUDFLARE_API_BASE_URL=<API URL>/client/v4\nexport CLOUDFLARE_API_TOKEN=${created.value}\nexport CLOUDFLARE_ACCOUNT_ID=${account}`}
              />
              <span>{t("settings.s3Keys")}</span>
              <ClipboardText text={created.s3.access_key_id} size="sm" />
              <ClipboardText text={created.s3.secret_access_key} size="sm" masked />
            </div>
          }
          action={
            <Button size="sm" variant="secondary" onClick={() => setCreated(undefined)}>
              {t("settings.tokenSaved")}
            </Button>
          }
        />
      )}
      <Section
        title={t("settings.tokens")}
        description={t("settings.tokensHelp")}
        actions={
          <Button size="sm" icon={PlusIcon} onClick={() => setCreating(true)}>
            {t("settings.createToken")}
          </Button>
        }
        flush
      >
        <Async
          query={tokens}
          empty={(l) => l.length === 0}
          emptyState={<EmptyState icon={<KeyIcon size={32} />} title={t("settings.noTokens")} />}
        >
          {(list) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("common.name")}</Table.Head>
                  <Table.Head>{t("settings.createdBy")}</Table.Head>
                  <Table.Head>{t("common.created")}</Table.Head>
                  <Table.Head />
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {list.map((tk) => (
                  <Table.Row key={tk.id}>
                    <Table.Cell>{tk.name}</Table.Cell>
                    <Table.Cell>
                      <span className="flex flex-wrap items-center gap-2">
                        {who(tk.created_by, t("settings.byToken"))}
                        {tk.creator_is_member === false && <Badge variant="warning">{t("settings.creatorLeft")}</Badge>}
                      </span>
                    </Table.Cell>
                    <Table.Cell>
                      <Time value={tk.created_on} />
                    </Table.Cell>
                    <Table.Cell className="w-px text-right whitespace-nowrap">
                      <Button variant="ghost" size="sm" shape="square" aria-label={t("settings.revoke", { name: tk.name })} onClick={() => setRevoking(tk)}>
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
      <CreateToken open={creating} onOpenChange={setCreating} onCreated={setCreated} />
      <ConfirmDelete
        open={revoking !== undefined}
        onOpenChange={(o) => !o && setRevoking(undefined)}
        kind={t("kinds.token")}
        name={revoking?.name ?? ""}
        confirmLabel={t("settings.revokeButton")}
        consequence={t("settings.revokeConsequence")}
        onConfirm={async () => {
          await revoke.mutateAsync(revoking!.id);
        }}
      />
    </div>
  );
}

function CreateToken(props: { open: boolean; onOpenChange: (o: boolean) => void; onCreated: (t: NewToken) => void }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const [name, setName] = useState("");
  const create = useChange(() => call<NewToken>("/selvedge/tokens", { method: "POST", json: { name: name.trim() } }), {
    invalidate: [["tokens"]],
    silentError: true,
  });
  return (
    <FormDialog
      open={props.open}
      onOpenChange={(o) => {
        props.onOpenChange(o);
        if (!o) setName("");
      }}
      title={t("settings.createToken")}
      description={t("settings.createTokenHelp")}
      submitLabel={t("common.create")}
      disabled={!name.trim()}
      onSubmit={async () => {
        props.onCreated(await create.mutateAsync());
      }}
    >
      <Input label={t("common.name")} placeholder={t("settings.tokenNameHint")} value={name} onChange={(e) => setName(e.target.value)} autoComplete="off" />
    </FormDialog>
  );
}

function Danger({ name }: { name?: string }) {
  const { t } = useTranslation();
  const { account, call } = useAccountApi();
  const navigate = useNavigate();
  const [deleting, setDeleting] = useState(false);
  const remove = useChange(() => call("", { method: "DELETE" }), { silentError: true });
  return (
    <Section title={t("settings.deleteAccount")} description={t("settings.deleteAccountHelp")}>
      <div>
        <Button variant="destructive" icon={TrashIcon} onClick={() => setDeleting(true)} disabled={!name}>
          {t("settings.deleteAccount")}
        </Button>
      </div>
      <ConfirmDelete
        open={deleting}
        onOpenChange={setDeleting}
        kind={t("kinds.account")}
        name={name ?? account}
        consequence={t("settings.deleteAccountConsequence", { name })}
        onConfirm={async () => {
          await remove.mutateAsync();
          await queryClient.invalidateQueries({ queryKey: ["me"] });
          navigate({ to: "/" });
        }}
      />
    </Section>
  );
}
