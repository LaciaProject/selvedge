import { Breadcrumbs } from "@cloudflare/kumo/components/breadcrumbs";
import { Button } from "@cloudflare/kumo/components/button";
import { ClipboardText } from "@cloudflare/kumo/components/clipboard-text";
import { Input, InputArea } from "@cloudflare/kumo/components/input";
import { Select } from "@cloudflare/kumo/components/select";
import { Switch } from "@cloudflare/kumo/components/switch";
import { Table } from "@cloudflare/kumo/components/table";
import { DownloadSimpleIcon, FileIcon, FolderIcon, PlusIcon, TrashIcon, UploadSimpleIcon } from "@phosphor-icons/react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { Fragment, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { Crumbs } from "../../components/Crumbs";
import { CursorPager, useCursors } from "../../components/CursorPager";
import { ConfirmDelete, FormDialog } from "../../components/dialogs";
import { UrlTabs } from "../../components/TabbedPage";
import { Async, EmptyState, ErrorBanner, Loading, Mono, Page, Section, Time } from "../../components/ui";
import { accountPath, api, apiPage, apiRaw, encodeKey } from "../../lib/api";
import { useFormat } from "../../lib/format";
import { useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import { notifyDone, notifyFailed } from "../../lib/toast";
import type { R2CustomDomain, R2DevUrl, R2ListInfo, R2NotificationQueue, R2Object, Zone } from "../../lib/types";

export function Bucket() {
  const { t } = useTranslation();
  const { bucket } = useParams({ strict: false }) as { bucket: string };
  const search = useSearch({ strict: false }) as { tab?: string; prefix?: string };
  const { account, call } = useAccountApi();
  const navigate = useNavigate();
  const [deleting, setDeleting] = useState(false);
  const remove = useChange(() => call(`/r2/buckets/${encodeURIComponent(bucket)}`, { method: "DELETE" }), {
    invalidate: [["r2"]],
    silentError: true,
  });
  return (
    <Page
      crumbs={<Crumbs items={[{ label: t("nav.r2"), href: `/accounts/${account}/r2` }]} current={bucket} />}
      title={bucket}
      actions={
        <Button variant="secondary-destructive" icon={TrashIcon} onClick={() => setDeleting(true)}>
          {t("common.delete")}
        </Button>
      }
    >
      <UrlTabs
        current={search.tab}
        tabs={[
          { value: "objects", label: t("r2.tabs.objects"), render: () => <Objects bucket={bucket} prefix={search.prefix ?? ""} /> },
          { value: "settings", label: t("r2.tabs.settings"), render: () => <BucketSettings bucket={bucket} /> },
        ]}
      />
      <ConfirmDelete
        open={deleting}
        onOpenChange={setDeleting}
        kind={t("kinds.bucket")}
        name={bucket}
        consequence={t("r2.deleteConsequence", { name: bucket })}
        onConfirm={async () => {
          await remove.mutateAsync();
          navigate({ to: "/accounts/$account/r2", params: { account } });
        }}
      />
    </Page>
  );
}

function Objects({ bucket, prefix }: { bucket: string; prefix: string }) {
  const { t } = useTranslation();
  const f = useFormat();
  const { account, call } = useAccountApi();
  const navigate = useNavigate();
  const pages = useCursors();
  const base = `/r2/buckets/${encodeURIComponent(bucket)}/objects`;
  const listing = useQuery({
    queryKey: [account, "r2-objects", bucket, prefix, pages.cursor],
    queryFn: () =>
      apiPage<R2Object[]>(accountPath(account, base), {
        query: { prefix, delimiter: "/", cursor: pages.cursor, per_page: 200 },
      }),
    placeholderData: keepPreviousData,
  });
  const info = listing.data?.result_info as unknown as R2ListInfo | undefined;
  const [uploading, setUploading] = useState(false);
  const [removing, setRemoving] = useState<string>();
  const file = useRef<HTMLInputElement>(null);
  const go = (p: string) => {
    pages.reset();
    navigate({ to: ".", search: (s: Record<string, unknown>) => ({ ...s, tab: "objects", prefix: p || undefined }) });
  };
  const remove = useChange((key: string) => call(`${base}/${encodeKey(key)}`, { method: "DELETE" }), {
    done: (_, key) => t("r2.objectDeleted", { name: key }),
    invalidate: [["r2-objects", bucket]],
    silentError: true,
  });

  const upload = async (files: FileList) => {
    setUploading(true);
    try {
      for (const file of Array.from(files)) {
        await call(`${base}/${encodeKey(prefix + file.name)}`, {
          method: "PUT",
          body: file,
          headers: { "content-type": file.type || "application/octet-stream" },
        });
      }
      notifyDone(t("r2.uploaded", { count: files.length }));
      listing.refetch();
    } catch (e) {
      notifyFailed(e, t);
    } finally {
      setUploading(false);
      if (file.current) file.current.value = "";
    }
  };

  const download = async (key: string) => {
    try {
      const res = await apiRaw(accountPath(account, `${base}/${encodeKey(key)}`));
      const url = URL.createObjectURL(await res.blob());
      const a = document.createElement("a");
      a.href = url;
      a.download = key.split("/").pop() || key;
      a.click();
      setTimeout(() => URL.revokeObjectURL(url), 10_000);
    } catch (e) {
      notifyFailed(e, t);
    }
  };

  const parts = prefix.split("/").filter(Boolean);
  return (
    <Section
      title={
        <Breadcrumbs size="sm">
          {parts.length === 0 ? (
            <Breadcrumbs.Current>{bucket}</Breadcrumbs.Current>
          ) : (
            <>
              <button type="button" className="text-kumo-link" onClick={() => go("")}>
                {bucket}
              </button>
              {parts.map((p, i) => (
                <Fragment key={i}>
                  <Breadcrumbs.Separator />
                  {i === parts.length - 1 ? (
                    <Breadcrumbs.Current>{p}</Breadcrumbs.Current>
                  ) : (
                    <button type="button" className="text-kumo-link" onClick={() => go(parts.slice(0, i + 1).join("/") + "/")}>
                      {p}
                    </button>
                  )}
                </Fragment>
              ))}
            </>
          )}
        </Breadcrumbs>
      }
      actions={
        <>
          <input ref={file} type="file" multiple className="hidden" onChange={(e) => e.target.files?.length && upload(e.target.files)} />
          <Button size="sm" icon={UploadSimpleIcon} loading={uploading} onClick={() => file.current?.click()}>
            {t("r2.upload")}
          </Button>
        </>
      }
      flush
    >
      {listing.isPending ? (
        <Loading />
      ) : listing.isError ? (
        <ErrorBanner error={listing.error} onRetry={() => listing.refetch()} />
      ) : listing.data.result.length === 0 && !info?.delimited?.length ? (
        <EmptyState title={t("r2.noObjects")} description={t("r2.noObjectsHelp")} />
      ) : (
        <>
          <Table>
            <Table.Header>
              <Table.Row>
                <Table.Head>{t("common.name")}</Table.Head>
                <Table.Head>{t("r2.size")}</Table.Head>
                <Table.Head>{t("r2.type")}</Table.Head>
                <Table.Head>{t("r2.modified")}</Table.Head>
                <Table.Head />
              </Table.Row>
            </Table.Header>
            <Table.Body>
              {(info?.delimited ?? []).map((p) => (
                <Table.Row key={p}>
                  <Table.Cell>
                    <button type="button" className="flex items-center gap-2 text-kumo-link" onClick={() => go(p)}>
                      <FolderIcon size={16} />
                      {p.slice(prefix.length)}
                    </button>
                  </Table.Cell>
                  <Table.Cell />
                  <Table.Cell />
                  <Table.Cell />
                  <Table.Cell />
                </Table.Row>
              ))}
              {listing.data.result.map((o) => (
                <Table.Row key={o.key}>
                  <Table.Cell>
                    <span className="flex items-center gap-2">
                      <FileIcon size={16} className="text-kumo-subtle" />
                      <Mono>{o.key.slice(prefix.length)}</Mono>
                    </span>
                  </Table.Cell>
                  <Table.Cell>{f.bytes(o.size)}</Table.Cell>
                  <Table.Cell className="text-kumo-subtle">{o.http_metadata?.contentType ?? "—"}</Table.Cell>
                  <Table.Cell>
                    <Time value={o.last_modified} />
                  </Table.Cell>
                  <Table.Cell className="w-px text-right whitespace-nowrap">
                    <Button variant="ghost" size="sm" shape="square" aria-label={t("r2.download", { name: o.key })} onClick={() => download(o.key)}>
                      <DownloadSimpleIcon size={16} />
                    </Button>
                    <Button variant="ghost" size="sm" shape="square" aria-label={t("r2.deleteObject", { name: o.key })} onClick={() => setRemoving(o.key)}>
                      <TrashIcon size={16} />
                    </Button>
                  </Table.Cell>
                </Table.Row>
              ))}
            </Table.Body>
          </Table>
          <CursorPager
            page={pages.page}
            hasNext={!!info?.is_truncated && !!info.cursor}
            onNext={() => pages.next(info!.cursor!)}
            onBack={pages.back}
          />
        </>
      )}
      <ConfirmDelete
        open={removing !== undefined}
        onOpenChange={(o) => !o && setRemoving(undefined)}
        kind={t("kinds.object")}
        name={removing ?? ""}
        onConfirm={async () => {
          await remove.mutateAsync(removing!);
        }}
      />
    </Section>
  );
}

function BucketSettings({ bucket }: { bucket: string }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const b = `/r2/buckets/${encodeURIComponent(bucket)}`;
  const dev = useAccountQuery<R2DevUrl>(["r2-dev", bucket], `${b}/domains/managed`);
  const custom = useAccountQuery<{ domains: R2CustomDomain[] }>(["r2-custom", bucket], `${b}/domains/custom`);
  const lifecycle = useAccountQuery<{ rules: unknown[] }>(["r2-lifecycle", bucket], `${b}/lifecycle`);
  const notifications = useAccountQuery<{ queues: R2NotificationQueue[] }>(
    ["r2-notifications", bucket],
    `/event_notifications/r2/${encodeURIComponent(bucket)}/configuration`,
  );
  const [addingDomain, setAddingDomain] = useState(false);
  const [removingDomain, setRemovingDomain] = useState<string>();
  const [editingRules, setEditingRules] = useState(false);

  const toggleDev = useChange((enabled: boolean) => call(`${b}/domains/managed`, { method: "PUT", json: { enabled } }), {
    done: (_, on) => (on ? t("r2.publicOn") : t("r2.publicOff")),
    invalidate: [["r2-dev", bucket]],
  });
  const removeDomain = useChange(
    (domain: string) => call(`${b}/domains/custom/${encodeURIComponent(domain)}`, { method: "DELETE" }),
    { invalidate: [["r2-custom", bucket]], silentError: true },
  );

  return (
    <div className="flex flex-col gap-6">
      <Section title={t("r2.publicAccess")} description={t("r2.publicAccessHelp")}>
        <Async query={dev}>
          {(d) => (
            <div className="flex flex-col gap-3">
              <Switch
                label={t("r2.publicUrl")}
                checked={d.enabled}
                transitioning={toggleDev.isPending}
                onClick={() => toggleDev.mutate(!d.enabled)}
              />
              {d.enabled && d.domain && <ClipboardText text={`https://${d.domain}`} size="base" />}
            </div>
          )}
        </Async>
      </Section>
      <Section
        title={t("r2.customDomains")}
        description={t("r2.customDomainsHelp")}
        actions={
          <Button size="sm" icon={PlusIcon} onClick={() => setAddingDomain(true)}>
            {t("r2.addDomain")}
          </Button>
        }
        flush
      >
        <Async query={custom} empty={(c) => c.domains.length === 0} emptyState={<EmptyState title={t("r2.noDomains")} />}>
          {(c) => (
            <Table>
              <Table.Body>
                {c.domains.map((d) => (
                  <Table.Row key={d.domain}>
                    <Table.Cell>
                      <Mono>{d.domain}</Mono>
                    </Table.Cell>
                    <Table.Cell className="text-kumo-subtle">{d.zoneName}</Table.Cell>
                    <Table.Cell className="w-px text-right whitespace-nowrap">
                      <Button variant="ghost" size="sm" shape="square" aria-label={t("r2.removeDomain", { name: d.domain })} onClick={() => setRemovingDomain(d.domain)}>
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
      <Section
        title={t("r2.lifecycle")}
        description={t("r2.lifecycleHelp")}
        actions={
          <Button size="sm" onClick={() => setEditingRules(true)} disabled={!lifecycle.data}>
            {t("common.edit")}
          </Button>
        }
      >
        <Async query={lifecycle}>
          {(l) =>
            l.rules.length === 0 ? (
              <span className="text-kumo-subtle">{t("r2.noRules")}</span>
            ) : (
              <pre className="overflow-x-auto font-mono text-xs">{JSON.stringify(l.rules, null, 2)}</pre>
            )
          }
        </Async>
      </Section>
      <Section title={t("r2.notifications")} description={t("r2.notificationsHelp")} flush>
        <Async
          query={notifications}
          empty={(n) => n.queues.length === 0}
          emptyState={<EmptyState title={t("r2.noNotifications")} />}
        >
          {(n) => (
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("r2.queue")}</Table.Head>
                  <Table.Head>{t("r2.events")}</Table.Head>
                  <Table.Head>{t("r2.filter")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {n.queues.flatMap((q) =>
                  q.rules.map((r) => (
                    <Table.Row key={r.ruleId}>
                      <Table.Cell>{q.queueName}</Table.Cell>
                      <Table.Cell>{r.actions.join(", ")}</Table.Cell>
                      <Table.Cell>
                        <Mono>{[r.prefix && `${r.prefix}*`, r.suffix && `*${r.suffix}`].filter(Boolean).join(" ") || "*"}</Mono>
                      </Table.Cell>
                    </Table.Row>
                  )),
                )}
              </Table.Body>
            </Table>
          )}
        </Async>
      </Section>
      <AddCustomDomain bucket={bucket} open={addingDomain} onOpenChange={setAddingDomain} />
      <ConfirmDelete
        open={removingDomain !== undefined}
        onOpenChange={(o) => !o && setRemovingDomain(undefined)}
        kind={t("kinds.domain")}
        name={removingDomain ?? ""}
        confirmLabel={t("common.remove")}
        consequence={t("r2.removeDomainConsequence")}
        onConfirm={async () => {
          await removeDomain.mutateAsync(removingDomain!);
        }}
      />
      {editingRules && lifecycle.data && (
        <LifecycleEditor bucket={bucket} rules={lifecycle.data.rules} onClose={() => setEditingRules(false)} />
      )}
    </div>
  );
}

function AddCustomDomain({ bucket, open, onOpenChange }: { bucket: string; open: boolean; onOpenChange: (o: boolean) => void }) {
  const { t } = useTranslation();
  const { account, call } = useAccountApi();
  const zones = useQuery({
    queryKey: [account, "zones"],
    queryFn: () => api<Zone[]>("/client/v4/zones", { query: { "account.id": account } }),
    enabled: open,
  });
  const [zone, setZone] = useState("");
  const [domain, setDomain] = useState("");
  const add = useChange(
    () =>
      call(`/r2/buckets/${encodeURIComponent(bucket)}/domains/custom`, {
        method: "POST",
        json: { domain: domain.trim(), zoneId: zone, enabled: true },
      }),
    { done: t("r2.domainAdded", { name: domain.trim() }), invalidate: [["r2-custom", bucket]], silentError: true },
  );
  return (
    <FormDialog
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) setDomain("");
      }}
      title={t("r2.addDomain")}
      description={t("r2.addDomainHelp")}
      submitLabel={t("common.add")}
      disabled={!zone || !domain.trim()}
      onSubmit={async () => {
        await add.mutateAsync();
      }}
    >
      {zones.data && zones.data.length === 0 ? (
        <p className="text-kumo-subtle">{t("r2.noZones")}</p>
      ) : (
        <Select
          label={t("r2.zone")}
          hideLabel={false}
          value={zone}
          onValueChange={(v) => setZone(String(v))}
          items={Object.fromEntries((zones.data ?? []).map((z) => [z.id, z.name]))}
          loading={zones.isPending}
        >
          {(zones.data ?? []).map((z) => (
            <Select.Option key={z.id} value={z.id}>
              {z.name}
            </Select.Option>
          ))}
        </Select>
      )}
      <Input label={t("r2.domain")} placeholder="files.example.com" value={domain} onChange={(e) => setDomain(e.target.value)} />
    </FormDialog>
  );
}

function LifecycleEditor({ bucket, rules, onClose }: { bucket: string; rules: unknown[]; onClose: () => void }) {
  const { t } = useTranslation();
  const { call } = useAccountApi();
  const [text, setText] = useState(JSON.stringify(rules, null, 2));
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    parsed = undefined;
  }
  const put = useChange(() => call(`/r2/buckets/${encodeURIComponent(bucket)}/lifecycle`, { method: "PUT", json: { rules: parsed } }), {
    done: t("r2.rulesSaved"),
    invalidate: [["r2-lifecycle", bucket]],
    silentError: true,
  });
  return (
    <FormDialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={t("r2.lifecycle")}
      description={t("r2.lifecycleEditHelp")}
      size="xl"
      submitLabel={t("common.save")}
      disabled={!Array.isArray(parsed)}
      onSubmit={async () => {
        await put.mutateAsync();
      }}
    >
      <InputArea
        aria-label={t("r2.lifecycle")}
        value={text}
        onValueChange={setText}
        rows={14}
        spellCheck={false}
        className="font-mono text-xs"
        error={Array.isArray(parsed) ? undefined : t("r2.rulesInvalid")}
      />
    </FormDialog>
  );
}
