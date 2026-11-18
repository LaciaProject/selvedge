import { Button } from "@cloudflare/kumo/components/button";
import { Input, InputArea } from "@cloudflare/kumo/components/input";
import { Table } from "@cloudflare/kumo/components/table";
import { MagnifyingGlassIcon, PlusIcon, TrashIcon } from "@phosphor-icons/react";
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useNavigate, useParams } from "@tanstack/react-router";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { Crumbs } from "../../components/Crumbs";
import { CursorPager, useCursors } from "../../components/CursorPager";
import { ConfirmDelete, FormDialog } from "../../components/dialogs";
import { EmptyState, ErrorBanner, Loading, Mono, Page, Section, Time } from "../../components/ui";
import { accountPath, api, apiPage, apiRaw, encodeKey } from "../../lib/api";
import { useAccountApi, useAccountQuery, useChange } from "../../lib/hooks";
import type { KvKey, KvNamespace } from "../../lib/types";

export function Namespace() {
  const { t } = useTranslation();
  const { namespace } = useParams({ strict: false }) as { namespace: string };
  const { account, call } = useAccountApi();
  const navigate = useNavigate();
  const base = `/storage/kv/namespaces/${namespace}`;
  const info = useAccountQuery<KvNamespace>(["kv", namespace], base);
  const [prefix, setPrefix] = useState("");
  const [search, setSearch] = useState("");
  const pages = useCursors();
  const keys = useQuery({
    queryKey: [account, "kv-keys", namespace, search, pages.cursor],
    queryFn: () =>
      apiPage<KvKey[]>(accountPath(account, `${base}/keys`), { query: { prefix: search, cursor: pages.cursor, limit: 100 } }),
    placeholderData: keepPreviousData,
  });
  const [open, setOpen] = useState<string | null>(null); // a key, or "" for a new one
  const [deleting, setDeleting] = useState(false);
  const removeNs = useChange(() => call(base, { method: "DELETE" }), { invalidate: [["kv"]], silentError: true });
  const title = info.data?.title ?? namespace;

  return (
    <Page
      crumbs={<Crumbs items={[{ label: t("nav.kv"), href: `/accounts/${account}/kv` }]} current={title} />}
      title={title}
      description={<Mono className="text-kumo-subtle">{namespace}</Mono>}
      actions={
        <>
          <Button variant="primary" icon={PlusIcon} onClick={() => setOpen("")}>
            {t("kv.addKey")}
          </Button>
          <Button variant="secondary-destructive" icon={TrashIcon} onClick={() => setDeleting(true)}>
            {t("common.delete")}
          </Button>
        </>
      }
    >
      <form
        className="flex gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          pages.reset();
          setSearch(prefix);
        }}
      >
        <Input
          aria-label={t("kv.prefix")}
          placeholder={t("kv.prefix")}
          value={prefix}
          onChange={(e) => setPrefix(e.target.value)}
          className="flex-1"
        />
        <Button type="submit" icon={MagnifyingGlassIcon}>
          {t("common.search")}
        </Button>
      </form>
      <Section title={t("kv.keys")} flush>
        {keys.isPending ? (
          <Loading />
        ) : keys.isError ? (
          <ErrorBanner error={keys.error} onRetry={() => keys.refetch()} />
        ) : keys.data.result.length === 0 ? (
          <EmptyState title={search ? t("kv.noMatch") : t("kv.noKeys")} />
        ) : (
          <>
            <Table>
              <Table.Header>
                <Table.Row>
                  <Table.Head>{t("kv.key")}</Table.Head>
                  <Table.Head>{t("kv.expires")}</Table.Head>
                </Table.Row>
              </Table.Header>
              <Table.Body>
                {keys.data.result.map((k) => (
                  <Table.Row key={k.name}>
                    <Table.Cell>
                      <button type="button" className="text-left text-kumo-link hover:underline" onClick={() => setOpen(k.name)}>
                        <Mono>{k.name}</Mono>
                      </button>
                    </Table.Cell>
                    <Table.Cell>{k.expiration ? <Time value={k.expiration * 1000} /> : <span className="text-kumo-subtle">{t("kv.never")}</span>}</Table.Cell>
                  </Table.Row>
                ))}
              </Table.Body>
            </Table>
            <CursorPager
              page={pages.page}
              hasNext={!!keys.data.result_info?.cursor}
              onNext={() => pages.next(keys.data.result_info!.cursor!)}
              onBack={pages.back}
            />
          </>
        )}
      </Section>
      {open !== null && <KeyEditor namespace={namespace} keyName={open} onClose={() => setOpen(null)} />}
      <ConfirmDelete
        open={deleting}
        onOpenChange={setDeleting}
        kind={t("kinds.kvNamespace")}
        name={title}
        onConfirm={async () => {
          await removeNs.mutateAsync();
          navigate({ to: "/accounts/$account/kv", params: { account } });
        }}
      />
    </Page>
  );
}

/** Shows a key's value and lets it be changed or deleted; with `keyName` "" it adds one. */
function KeyEditor({ namespace, keyName, onClose }: { namespace: string; keyName: string; onClose: () => void }) {
  const { t } = useTranslation();
  const { account, call } = useAccountApi();
  const base = `/storage/kv/namespaces/${namespace}`;
  const isNew = keyName === "";
  const value = useQuery({
    queryKey: [account, "kv-value", namespace, keyName],
    queryFn: async () => {
      const res = await apiRaw(accountPath(account, `${base}/values/${encodeKey(keyName)}`));
      const bytes = new Uint8Array(await res.arrayBuffer());
      const text = new TextDecoder("utf-8", { fatal: false }).decode(bytes);
      return { text, binary: text.includes("�"), size: bytes.length };
    },
    enabled: !isNew,
  });
  const metadata = useQuery({
    queryKey: [account, "kv-metadata", namespace, keyName],
    queryFn: () => api<unknown>(accountPath(account, `${base}/metadata/${encodeKey(keyName)}`)),
    enabled: !isNew,
  });
  const [meta, setMeta] = useState<string>();
  const metaText = meta ?? (metadata.data === undefined || metadata.data === null ? "" : JSON.stringify(metadata.data, null, 2));
  let metaValid = true;
  try {
    if (metaText.trim()) JSON.parse(metaText);
  } catch {
    metaValid = false;
  }
  const [name, setName] = useState(keyName);
  const [draft, setDraft] = useState<string>();
  const [ttl, setTtl] = useState("");
  const [deleting, setDeleting] = useState(false);
  const put = useChange(
    () => {
      const text = draft ?? value.data?.text ?? "";
      // Metadata goes with the value in a multipart body, as the API takes it.
      let body: BodyInit = text;
      const headers: Record<string, string> = {};
      if (metaText.trim()) {
        const form = new FormData();
        form.set("value", new Blob([text]));
        form.set("metadata", JSON.stringify(JSON.parse(metaText)));
        body = form;
      } else {
        headers["content-type"] = "text/plain";
      }
      return call(`${base}/values/${encodeKey(name.trim())}`, {
        method: "PUT",
        body,
        headers,
        query: { expiration_ttl: ttl.trim() || undefined },
      });
    },
    {
      done: t("kv.saved", { name: name.trim() }),
      invalidate: [["kv-keys", namespace], ["kv-value", namespace], ["kv-metadata", namespace]],
      silentError: true,
    },
  );
  const remove = useChange(() => call(`${base}/values/${encodeKey(keyName)}`, { method: "DELETE" }), {
    done: t("kv.keyDeleted", { name: keyName }),
    invalidate: [["kv-keys", namespace]],
    silentError: true,
  });
  const ttlValid = ttl.trim() === "" || (/^\d+$/.test(ttl.trim()) && Number(ttl) >= 60);

  return (
    <>
      <FormDialog
        open={!deleting}
        onOpenChange={(o) => !o && onClose()}
        title={isNew ? t("kv.addKey") : keyName}
        size="xl"
        submitLabel={t("common.save")}
        disabled={!name.trim() || !ttlValid || !metaValid || value.data?.binary === true}
        onSubmit={async () => {
          await put.mutateAsync();
        }}
      >
        {isNew && <Input label={t("kv.key")} value={name} onChange={(e) => setName(e.target.value)} autoComplete="off" />}
        {!isNew && value.isPending && <Loading />}
        {!isNew && value.isError && <ErrorBanner error={value.error} />}
        {value.data?.binary ? (
          <p className="text-kumo-subtle">{t("kv.binary", { size: value.data.size })}</p>
        ) : (
          (isNew || value.data) && (
            <InputArea
              label={t("kv.value")}
              value={draft ?? value.data?.text ?? ""}
              onValueChange={setDraft}
              rows={10}
              spellCheck={false}
              className="font-mono"
            />
          )
        )}
        <InputArea
          label={t("kv.metadata")}
          description={t("kv.metadataHelp")}
          value={metaText}
          onValueChange={setMeta}
          rows={3}
          spellCheck={false}
          className="font-mono"
          error={metaValid ? undefined : t("queues.invalidJson")}
        />
        <Input
          label={t("kv.ttl")}
          description={t("kv.ttlHelp")}
          value={ttl}
          onChange={(e) => setTtl(e.target.value)}
          inputMode="numeric"
          error={ttlValid ? undefined : t("kv.ttlInvalid")}
        />
        {!isNew && (
          <div>
            <Button type="button" variant="secondary-destructive" size="sm" icon={TrashIcon} onClick={() => setDeleting(true)}>
              {t("kv.deleteKey")}
            </Button>
          </div>
        )}
      </FormDialog>
      <ConfirmDelete
        open={deleting}
        onOpenChange={(o) => {
          setDeleting(o);
        }}
        kind={t("kinds.kvKey")}
        name={keyName}
        onConfirm={async () => {
          await remove.mutateAsync();
          onClose();
        }}
      />
    </>
  );
}

